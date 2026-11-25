//! The facts of an action ([`Inputs::action`] of [`Front`]): from the Action and the Command the
//! Execute that opened the operation decoded, or read from the CAS; remembered by the action's
//! digest. The module docs of `front.rs` say what they are.

use std::time::Duration;

use kbf_front::{DecodedAction, Described};
use kbf_proto::reapi::{self, Action, Command, Digest};
use kbf_types::ImageRef;

use super::{Front, cas_digest};
use crate::action_usage::Shape;
use crate::executor::mac::System;
use crate::executor::size_class::SizeClass;
use crate::executor::task::RunId;
use crate::executor::traits::{ActionFacts, Inputs, InputsError};

/// The name of the platform property that names the image.
const IMAGE_PROPERTY: &str = "container-image";

#[tonic::async_trait]
impl Inputs for Front {
    async fn action(
        &self,
        action: &Digest,
        decoded: Option<&DecodedAction>,
    ) -> Result<ActionFacts, InputsError> {
        let key = (action.hash.clone(), action.size_bytes);
        let remembered = self.facts().get(&key);
        if let Some(facts) = remembered {
            return Ok(facts);
        }
        let facts = match decoded {
            Some(decoded) => self.facts_of(&decoded.action, &decoded.command)?,
            None => self.read_facts(action).await?,
        };
        self.facts().put(key, facts.clone());
        Ok(facts)
    }

    async fn tree(&self, run: RunId) -> Result<String, InputsError> {
        let parked = self.parked().remove(&run);
        parked
            .map(|parked| parked.tree)
            .ok_or_else(|| InputsError::Unavailable(format!("no tree was written for {run}")))
    }
}

impl Front {
    /// The facts of `action`, read from the CAS.
    async fn read_facts(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        let digest = cas_digest(action).map_err(InputsError::Invalid)?;
        let action: Action = self.read("action", &digest).await?;
        let command = action
            .command_digest
            .as_ref()
            .ok_or_else(|| InputsError::Invalid("the action names no command".to_owned()))?;
        let command = cas_digest(command).map_err(InputsError::Invalid)?;
        let command: Command = self.read("command", &command).await?;
        self.facts_of(&action, &command)
    }

    /// The facts of the Action `action`, whose Command is `command`.
    fn facts_of(&self, action: &Action, command: &Command) -> Result<ActionFacts, InputsError> {
        let platform = platform_of(action, command);
        check_base(platform, &self.base_image, &self.kbf_image)?;
        let timeout = timeout_of(action)?;
        let system = System::of(platform).map_err(InputsError::Invalid)?;
        // The target and kind of the action, read from its argv and output paths.
        let described = Described::of(command.clone());
        let shape = Shape::of(&described.arguments, &described.outputs);
        Ok(ActionFacts {
            image: self.kbf_image.to_string(),
            timeout,
            system,
            size_class: SizeClass::of_action(&shape),
        })
    }
}

/// Refuses an action whose `container-image` is not the base environment `base` this executor
/// serves. The property is read from the Action's platform or else the Command's; one that
/// names none means the base. Only the manifest digest is compared: two spellings of one digest
/// are one image. The refusal names both images, and the kbf image the run would use.
fn check_base(
    platform: Option<&reapi::Platform>,
    base: &ImageRef,
    kbf: &ImageRef,
) -> Result<(), InputsError> {
    let named = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == IMAGE_PROPERTY))
        .map(|p| p.value.as_str());
    let Some(value) = named else {
        return Ok(());
    };
    let image = ImageRef::parse(value).map_err(|e| InputsError::Invalid(e.to_string()))?;
    match image.digest() == base.digest() {
        true => Ok(()),
        false => Err(InputsError::Invalid(format!(
            "the action's {IMAGE_PROPERTY} {image} is not the base environment this executor \
             serves, {base} (runs are launched from the kbf image {kbf}, which is built from \
             it): name the served base, or none"
        ))),
    }
}

/// The platform of `action` if it names any property, or else of its `command`: as `kbf-front`
/// reads it (`kbf-front`'s `execution.rs`), so the system the executor runs an action on is the
/// one `kbf-front` read for it.
// The Command's platform is deprecated in REAPI 2.2 in favour of the Action's, and clients that
// predate it still send only the Command's: both are read, the Action's first.
#[allow(deprecated)]
fn platform_of<'a>(action: &'a Action, command: &'a Command) -> Option<&'a reapi::Platform> {
    action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref())
}

/// The action's timeout; none if it names none or zero.
fn timeout_of(action: &Action) -> Result<Option<Duration>, InputsError> {
    let Some(timeout) = &action.timeout else {
        return Ok(None);
    };
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(timeout.seconds), u32::try_from(timeout.nanos))
    else {
        return Err(InputsError::Invalid(
            "the action's timeout is negative".to_owned(),
        ));
    };
    let timeout = Duration::new(seconds, nanos);
    Ok((!timeout.is_zero()).then_some(timeout))
}
