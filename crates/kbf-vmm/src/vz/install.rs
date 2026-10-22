//! `image base`: VZMacOSRestoreImage and VZMacOSInstaller write a bundle.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSError, NSProgress};
use objc2_virtualization::{
    VZMacAuxiliaryStorage, VZMacAuxiliaryStorageInitializationOptions, VZMacOSInstaller,
    VZMacOSRestoreImage, VZVirtualMachine,
};
use serde_json::{Value, json};

use super::{Queued, config, describe, new_machine_identifier, url};
use crate::bundle::{AUX, DISK, FILES, HARDWARE_MODEL, MACHINE_ID};
use crate::spec::VmSpec;

/// What the install queue keeps alive.
struct Installing {
    _vm: Retained<VZVirtualMachine>,
    _installer: Retained<VZMacOSInstaller>,
}

/// Loads the restore image at `ipsw`.
fn load(ipsw: &Path) -> Result<Queued<Retained<VZMacOSRestoreImage>>, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |image: *mut VZMacOSRestoreImage, err: *mut NSError| {
        // SAFETY: non-null pointers are live objects for the duration of the call; the
        // image is retained before the call returns.
        let r = unsafe {
            match err.as_ref() {
                Some(e) => Err(describe(e).0),
                None => Retained::retain(image)
                    .map(Queued::new)
                    .ok_or_else(|| "the framework returned no restore image".to_owned()),
            }
        };
        let _ = tx.send(r);
    });
    // SAFETY: the URL and block are valid for the call; the framework copies the block.
    unsafe { VZMacOSRestoreImage::loadFileURL_completionHandler(&url(ipsw), &block) };
    rx.recv_timeout(Duration::from_secs(300))
        .map_err(|_| "loading the restore image timed out".to_owned())?
}

/// Installs macOS from `ipsw` into the bundle directory `out`.
///
/// # Errors
/// The restore image is unusable on this host or a step failed.
pub fn install(ipsw: &Path, out: &Path, disk_gib: u64) -> Result<Value, String> {
    let began = Instant::now();
    fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    if let Some(f) = FILES.iter().find(|f| out.join(f).exists()) {
        return Err(format!("{} already holds {f}", out.display()));
    }
    let image = load(ipsw)?;
    // SAFETY: reads on objects this function owns.
    let (hw_data, cpus, mem, version, build) = unsafe {
        let img = image.get();
        let req = img
            .mostFeaturefulSupportedConfiguration()
            .ok_or("this restore image has no configuration this host supports")?;
        let model = req.hardwareModel();
        if !model.isSupported() {
            return Err(
                "the restore image's hardware model is not supported on this host".to_owned(),
            );
        }
        let v = img.operatingSystemVersion();
        (
            model.dataRepresentation().to_vec(),
            req.minimumSupportedCPUCount().max(2),
            req.minimumSupportedMemorySize().max(4 << 30),
            format!("{}.{}.{}", v.majorVersion, v.minorVersion, v.patchVersion),
            img.buildVersion().to_string(),
        )
    };
    fs::write(out.join(HARDWARE_MODEL), &hw_data)
        .map_err(|e| format!("write hardware-model: {e}"))?;
    fs::write(out.join(MACHINE_ID), new_machine_identifier())
        .map_err(|e| format!("write machine-identifier: {e}"))?;
    File::create(out.join(DISK))
        .and_then(|f| f.set_len(disk_gib << 30))
        .map_err(|e| format!("create disk.img: {e}"))?;
    // SAFETY: creates the auxiliary storage file at a path nothing else uses; the model
    // was built from the restore image just above.
    unsafe {
        let model = objc2_virtualization::VZMacHardwareModel::initWithDataRepresentation(
            objc2_virtualization::VZMacHardwareModel::alloc(),
            &objc2_foundation::NSData::with_bytes(&hw_data),
        )
        .ok_or("the hardware model did not round-trip")?;
        VZMacAuxiliaryStorage::initCreatingStorageAtURL_hardwareModel_options_error(
            VZMacAuxiliaryStorage::alloc(),
            &url(&out.join(AUX)),
            &model,
            VZMacAuxiliaryStorageInitializationOptions::empty(),
        )
        .map_err(|e| describe(&e).0)?;
    }

    let mut spec = VmSpec::minimal(out.to_owned());
    spec.cpus = u32::try_from(cpus).unwrap_or(2);
    spec.memory_mib = mem >> 20;
    spec.graphics = true;
    let built = config::build(&spec)?;

    let queue = DispatchQueue::new("com.kbf.vmm.install", DispatchQueueAttr::SERIAL);
    let holder: Arc<Mutex<Option<Queued<Installing>>>> = Arc::new(Mutex::new(None));
    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
    let (progress_tx, progress_rx) = mpsc::channel::<Queued<Retained<NSProgress>>>();
    let cfg = Queued::new(built.config);
    let ipsw_url = Queued::new(url(ipsw));
    let (q, h) = (queue.clone(), holder.clone());
    queue.exec_async(move || {
        // SAFETY: on the install queue, which is the only place the VM and installer are
        // touched; both are kept alive in `holder` until the install ends.
        unsafe {
            let vm = VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                cfg.get(),
                &q,
            );
            let installer = VZMacOSInstaller::initWithVirtualMachine_restoreImageURL(
                VZMacOSInstaller::alloc(),
                &vm,
                ipsw_url.get(),
            );
            let _ = progress_tx.send(Queued::new(installer.progress()));
            let block = RcBlock::new(move |err: *mut NSError| {
                // A non-null `err` is live for the call.
                let r = match err.as_ref() {
                    None => Ok(()),
                    Some(e) => Err(describe(e).0),
                };
                let _ = done_tx.send(r);
            });
            installer.installWithCompletionHandler(&block);
            if let Ok(mut slot) = h.lock() {
                *slot = Some(Queued::new(Installing {
                    _vm: vm,
                    _installer: installer,
                }));
            }
        }
    });

    let finished = Arc::new(AtomicBool::new(false));
    if let Ok(progress) = progress_rx.recv_timeout(Duration::from_secs(60)) {
        let flag = finished.clone();
        std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                // NSProgress is documented as thread safe.
                let f = progress.get().fractionCompleted();
                let _ = writeln!(std::io::stderr(), "{{\"install_progress\":{f:.3}}}");
                std::thread::sleep(Duration::from_secs(15));
            }
        });
    }
    let result = done_rx
        .recv_timeout(Duration::from_secs(3 * 3600))
        .map_err(|_| "the install did not finish in 3 hours".to_owned());
    finished.store(true, Ordering::SeqCst);
    drop(holder);
    result??;
    Ok(json!({
        "macos_version": version,
        "macos_build": build,
        "cpus": cpus,
        "memory_mib": mem >> 20,
        "disk_gib": disk_gib,
        "install_seconds": began.elapsed().as_secs(),
    }))
}
