//! A `VZVirtualMachineConfiguration` from a [`VmSpec`]. There is no network device.

use std::fs;
use std::os::fd::RawFd;

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSFileHandle, NSString};
use objc2_virtualization::{
    VZConsoleDeviceConfiguration, VZDirectorySharingDeviceConfiguration,
    VZDiskImageStorageDeviceAttachment, VZFileHandleSerialPortAttachment,
    VZGraphicsDeviceConfiguration, VZMacAuxiliaryStorage, VZMacGraphicsDeviceConfiguration,
    VZMacGraphicsDisplayConfiguration, VZMacHardwareModel, VZMacMachineIdentifier,
    VZMacOSBootLoader, VZMacPlatformConfiguration, VZMultipleDirectoryShare, VZSharedDirectory,
    VZSingleDirectoryShare, VZSocketDeviceConfiguration, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceConfiguration,
    VZVirtioConsolePortConfiguration, VZVirtioFileSystemDeviceConfiguration,
    VZVirtioSocketDeviceConfiguration, VZVirtualMachineConfiguration,
};

use super::url;
use crate::bundle::{AUX, DISK, HARDWARE_MODEL, MACHINE_ID};
use crate::spec::VmSpec;

/// The host ends of one console port: the helper reads `from_guest` and keeps `_to_guest`
/// open so the guest's reads never see end of file.
pub struct ConsoleFds {
    pub name: String,
    pub from_guest: RawFd,
    pub _to_guest_write: RawFd,
}

/// A configuration plus the descriptors of its console ports.
pub struct Built {
    pub config: Retained<VZVirtualMachineConfiguration>,
    pub consoles: Vec<ConsoleFds>,
}

fn pipe() -> Result<(RawFd, RawFd), String> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

fn handle(fd: RawFd) -> Retained<NSFileHandle> {
    // The descriptor stays open for the helper's lifetime (see the module docs of `vz`),
    // and `closeOnDealloc: false` leaves closing to the helper.
    NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), fd, false)
}

fn data(path: &std::path::Path) -> Result<Retained<NSData>, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(NSData::with_bytes(&bytes))
}

/// Builds and validates the configuration.
///
/// # Errors
/// A bundle file is unreadable, a device cannot be made, or the framework's validation
/// refuses the configuration.
pub fn build(spec: &VmSpec) -> Result<Built, String> {
    let b = &spec.bundle;
    // SAFETY: every call below is an Objective-C message to an object this function owns
    // (`Retained`), with arguments that outlive the call; the framework copies what it
    // keeps. The configuration is only read by the VM created from it afterwards.
    unsafe {
        let cfg = VZVirtualMachineConfiguration::new();
        cfg.setCPUCount(spec.cpus as usize);
        cfg.setMemorySize(spec.memory_mib << 20);

        let model_bytes = data(&b.join(HARDWARE_MODEL))?;
        let ident_bytes = data(&b.join(MACHINE_ID))?;
        let model = VZMacHardwareModel::initWithDataRepresentation(
            VZMacHardwareModel::alloc(),
            &model_bytes,
        )
        .ok_or("hardware-model is not a hardware model")?;
        let ident = VZMacMachineIdentifier::initWithDataRepresentation(
            VZMacMachineIdentifier::alloc(),
            &ident_bytes,
        )
        .ok_or("machine-identifier is not a machine identifier")?;
        let aux =
            VZMacAuxiliaryStorage::initWithURL(VZMacAuxiliaryStorage::alloc(), &url(&b.join(AUX)));
        let platform = VZMacPlatformConfiguration::new();
        platform.setHardwareModel(&model);
        platform.setMachineIdentifier(&ident);
        platform.setAuxiliaryStorage(Some(&aux));
        cfg.setPlatform(&platform);
        cfg.setBootLoader(Some(&VZMacOSBootLoader::new()));

        let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &url(&b.join(DISK)),
            false,
        )
        .map_err(|e| super::describe(&e).0)?;
        let disk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment,
        );
        let storage: Retained<NSArray<VZStorageDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(disk)]);
        cfg.setStorageDevices(&storage);

        if spec.graphics {
            let display = VZMacGraphicsDisplayConfiguration::initWithWidthInPixels_heightInPixels_pixelsPerInch(
                VZMacGraphicsDisplayConfiguration::alloc(),
                1920,
                1080,
                80,
            );
            let gfx = VZMacGraphicsDeviceConfiguration::new();
            gfx.setDisplays(&NSArray::from_retained_slice(&[display]));
            let list: Retained<NSArray<VZGraphicsDeviceConfiguration>> =
                NSArray::from_retained_slice(&[Retained::into_super(gfx)]);
            cfg.setGraphicsDevices(&list);
        }

        let mut shares: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        if let Some(dir) = &spec.automount_dir {
            let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &VZVirtioFileSystemDeviceConfiguration::macOSGuestAutomountTag(),
            );
            let shared = VZSharedDirectory::initWithURL_readOnly(
                VZSharedDirectory::alloc(),
                &url(dir),
                false,
            );
            let dict: Retained<NSDictionary<NSString, VZSharedDirectory>> =
                NSDictionary::from_retained_objects(&[&*NSString::from_str("kbf")], &[shared]);
            let share = VZMultipleDirectoryShare::initWithDirectories(
                VZMultipleDirectoryShare::alloc(),
                &dict,
            );
            dev.setShare(Some(&share));
            shares.push(Retained::into_super(dev));
        }
        for t in &spec.tagged {
            let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str(&t.tag),
            );
            let shared = VZSharedDirectory::initWithURL_readOnly(
                VZSharedDirectory::alloc(),
                &url(&t.path),
                t.read_only,
            );
            if let Some(name) = &t.multiple_name {
                let dict: Retained<NSDictionary<NSString, VZSharedDirectory>> =
                    NSDictionary::from_retained_objects(&[&*NSString::from_str(name)], &[shared]);
                let share = VZMultipleDirectoryShare::initWithDirectories(
                    VZMultipleDirectoryShare::alloc(),
                    &dict,
                );
                dev.setShare(Some(&share));
            } else {
                let share = VZSingleDirectoryShare::initWithDirectory(
                    VZSingleDirectoryShare::alloc(),
                    &shared,
                );
                dev.setShare(Some(&share));
            }
            shares.push(Retained::into_super(dev));
        }
        if !shares.is_empty() {
            cfg.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
        }

        if !spec.vsock_ports.is_empty() {
            let dev = VZVirtioSocketDeviceConfiguration::new();
            let list: Retained<NSArray<VZSocketDeviceConfiguration>> =
                NSArray::from_retained_slice(&[Retained::into_super(dev)]);
            cfg.setSocketDevices(&list);
        }

        let mut consoles = Vec::new();
        if !spec.console_ports.is_empty() {
            let device = VZVirtioConsoleDeviceConfiguration::new();
            device
                .ports()
                .setMaximumPortCount(u32::try_from(spec.console_ports.len()).unwrap_or(1));
            for (i, name) in spec.console_ports.iter().enumerate() {
                let (from_guest, guest_writes) = pipe()?;
                let (guest_reads, to_guest_write) = pipe()?;
                let attachment = VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    Some(&handle(guest_reads)),
                    Some(&handle(guest_writes)),
                );
                let port = VZVirtioConsolePortConfiguration::new();
                port.setName(Some(&NSString::from_str(name)));
                port.setAttachment(Some(&attachment));
                device.ports().setObject_atIndexedSubscript(Some(&port), i);
                consoles.push(ConsoleFds {
                    name: name.clone(),
                    from_guest,
                    _to_guest_write: to_guest_write,
                });
            }
            let list: Retained<NSArray<VZConsoleDeviceConfiguration>> =
                NSArray::from_retained_slice(&[Retained::into_super(device)]);
            cfg.setConsoleDevices(&list);
        }

        cfg.validateWithError().map_err(|e| super::describe(&e).0)?;
        Ok(Built {
            config: cfg,
            consoles,
        })
    }
}
