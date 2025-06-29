//! [NodeActor] trait.

use async_trait::async_trait;
use tokio_util::sync::WaitForCancellationFuture;

/// The communication context used by the actor.
pub trait CancellableContext: Send {
    /// Returns a future that resolves when the actor is cancelled.
    fn cancelled(&self) -> WaitForCancellationFuture<'_>;
}

/// The [NodeActor] is an actor-like service for the node.
///
/// Actors may:
/// - Handle incoming messages.
///     - Perform background tasks.
/// - Emit new events for other actors to process.
#[async_trait]
pub trait NodeActor: Send + 'static {
    /// The error type for the actor.
    type Error: std::fmt::Debug;
    /// The communication context used by the actor.
    /// These are the channels that the actor will use to receive messages from other actors.
    type InboundData: CancellableContext;
    /// The outbound communication channels used by the actor.
    /// These are the channels that the actor will use to send messages to other actors.
    type OutboundData: Sized;
    /// The inner state for the actor.
    type State;

    /// Builds the actor.
    fn build(initial_state: Self::State) -> (Self::OutboundData, Self);

    /// Starts the actor.
    async fn start(self, inbound_context: Self::InboundData) -> Result<(), Self::Error>;
}
