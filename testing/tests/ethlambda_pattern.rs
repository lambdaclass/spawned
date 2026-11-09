//! The hand-written protocol stub pattern from Ethlambda, replaced by a mock.
//!
//! Ethlambda's `crates/net/p2p/src/beacon/column_checks.rs` tests implement
//! every method of `P2PToBlockChain` by hand on a `RecordingChain` struct
//! (eight methods, six of them `Ok(())`) and forward the two interesting ones
//! into an mpsc channel; `envelope_checks.rs` and `verdict.rs` repeat the
//! pattern. The production code holds a `P2PToBlockChainRef`, so a `Mock`
//! converted with the generated `to_..._ref()` replaces the stub with no
//! production change: new protocol methods need no test edits, and the
//! assertions read the typed protocol messages.
#![cfg(feature = "async")]
use spawned_concurrency::error::ActorError;
use spawned_concurrency::protocol;
use spawned_test::tasks::Mock;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Sidecar {
    pub slot: u64,
    pub parent_known: bool,
    pub well_formed: bool,
}

/// Trimmed copy of the shape of Ethlambda's `P2PToBlockChain`.
#[protocol]
pub trait P2PToBlockChain: Send + Sync {
    fn new_block(&self, slot: u64) -> Result<(), ActorError>;
    fn new_attestation(&self, slot: u64) -> Result<(), ActorError>;
    fn new_aggregated_attestation(&self, slot: u64) -> Result<(), ActorError>;
    fn new_data_column_sidecars(&self, sidecars: Vec<Sidecar>) -> Result<(), ActorError>;
    fn data_column_sidecars_awaiting_parent(
        &self,
        sidecars: Vec<Sidecar>,
    ) -> Result<(), ActorError>;
}

/// Production-shaped code: the p2p server holds a protocol reference.
struct Server {
    blockchain: Option<P2PToBlockChainRef>,
}

/// Check sidecars off the actor, forward what passed and park orphans.
fn check_and_forward(server: &Server, sidecars: Vec<Sidecar>) {
    let Some(chain) = server.blockchain.clone() else {
        return;
    };
    tokio::spawn(async move {
        let (checked, rest): (Vec<_>, Vec<_>) = sidecars.into_iter().partition(|s| s.parent_known);
        let orphans: Vec<_> = rest.into_iter().filter(|s| s.well_formed).collect();
        if !checked.is_empty() {
            let _ = chain.new_data_column_sidecars(checked);
        }
        if !orphans.is_empty() {
            let _ = chain.data_column_sidecars_awaiting_parent(orphans);
        }
    });
}

#[tokio::test]
async fn only_what_passes_is_forwarded_as_checked() {
    // Before: `struct RecordingChain(mpsc::UnboundedSender<Forwarded>)` plus a
    // full `impl P2PToBlockChain for RecordingChain`. After: one line.
    let chain = Mock::new().allow_sends().start();
    let server = Server {
        blockchain: Some(chain.to_p2p_to_block_chain_ref()),
    };

    let orphan = Sidecar {
        slot: 5,
        parent_known: false,
        well_formed: true,
    };
    let malformed = Sidecar {
        well_formed: false,
        ..orphan.clone()
    };
    check_and_forward(&server, vec![orphan.clone(), malformed]);

    let parked: p2p_to_block_chain::DataColumnSidecarsAwaitingParent =
        chain.expect(Duration::from_secs(2)).await;
    assert_eq!(parked.sidecars, vec![orphan]);
    // Nothing reached the batch the chain actor stores unchecked.
    chain.expect_no_message(Duration::from_millis(50)).await;
    assert_eq!(
        chain.count::<p2p_to_block_chain::NewDataColumnSidecars>(),
        0
    );
}
