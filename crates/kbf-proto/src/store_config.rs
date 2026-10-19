//! `kbf.enroll.v1.StoreConfig` carries the farm's cold-store secret, so the generated
//! type's derived `Debug` is switched off in `build.rs` and written here: every field but
//! the secret is printed, the secret as `Secret(..)`. A tonic or tower trace, a `{:?}` in
//! an error path or a failed `assert_eq!` therefore cannot print it. The secret is
//! overwritten when the message is dropped; copies the transport makes while encoding or
//! decoding are outside this type's reach.

use std::fmt;

use zeroize::Zeroize;

use crate::kbf::enroll::v1::StoreConfig;

impl fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreConfig")
            .field("connection", &self.connection)
            .field("role", &self.role)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("conditional_put", &self.conditional_put)
            .field("min_put_rate", &self.min_put_rate)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &format_args!("Secret(..)"))
            .field("secret_version", &self.secret_version)
            .finish()
    }
}

impl Drop for StoreConfig {
    fn drop(&mut self) {
        self.secret_access_key.zeroize();
    }
}
