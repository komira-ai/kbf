//! A store address that gives no answer, for the integration tests that need "a store that is
//! not there". The same as `DeadStore` of the unit tests (`src/cells/testkit.rs`), which an
//! integration binary cannot see.
//!
//! Use this and not a port bound and then closed: a closed port is handed to the next listener
//! any other test binary binds on this machine, and that test's fake store then answers the
//! requests meant for the dead one. Here the address stays bound for as long as the value lives,
//! and every connection made to it is closed at once, so a request to it ends without an answer.

pub struct DeadStore {
    url: String,
    accepting: tokio::task::JoinHandle<()>,
}

impl DeadStore {
    /// Binds a loopback address and holds it.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepting = tokio::spawn(async move {
            loop {
                drop(listener.accept().await.unwrap());
            }
        });
        Self { url, accepting }
    }

    /// The endpoint, as a store spec names it.
    pub fn endpoint(&self) -> &str {
        &self.url
    }
}

impl Drop for DeadStore {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::DeadStore;

    /// Catches a dead store whose port is free to be taken by another test's listener, which then
    /// answers for it: the next listener is asked for the very address.
    #[tokio::test]
    async fn the_address_of_a_dead_store_cannot_be_taken_by_another_listener() {
        let dead = DeadStore::start().await;
        let addr = dead.endpoint().trim_start_matches("http://").to_owned();
        let intruder = tokio::net::TcpListener::bind(&addr).await;
        assert!(intruder.is_err(), "another listener took {addr}");
    }
}
