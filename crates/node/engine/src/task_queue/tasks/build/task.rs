//! A task for building a new block and importing it.

use super::BuildTaskError;
use crate::{
    EngineClient, EngineForkchoiceVersion, EngineGetPayloadVersion, EngineState, EngineTaskError,
    EngineTaskExt, ForkchoiceTask, Metrics,
};
use alloy_provider::ext::EngineApi;
use alloy_rpc_types_engine::{
    ExecutionPayloadFieldV2, ExecutionPayloadInputV2, ForkchoiceState, PayloadId, PayloadStatusEnum,
};
use alloy_transport::RpcError;
use async_trait::async_trait;
use kona_genesis::RollupConfig;
use kona_protocol::{L2BlockInfo, OpAttributesWithParent};
use op_alloy_provider::ext::engine::OpEngineApi;
use op_alloy_rpc_types_engine::{OpExecutionPayload, OpExecutionPayloadEnvelope};
use std::{sync::Arc, time::Instant};
use tokio::sync::mpsc;

/// The [`BuildTask`] is responsible for building new blocks and importing them via the engine API.
#[derive(Debug, Clone)]
pub struct BuildTask {
    /// The engine API client.
    pub engine: Arc<EngineClient>,
    /// The [`RollupConfig`].
    pub cfg: Arc<RollupConfig>,
    /// The [`OpAttributesWithParent`] to instruct the execution layer to build.
    pub attributes: OpAttributesWithParent,
    /// Whether or not the payload was derived, or created by the sequencer.
    pub is_attributes_derived: bool,
    /// An optional channel to send the built [`OpExecutionPayloadEnvelope`] to, after the block
    /// has been built, imported, and canonicalized.
    pub payload_tx: Option<mpsc::Sender<OpExecutionPayloadEnvelope>>,
}

impl BuildTask {
    /// Creates a new block building task.
    pub const fn new(
        engine: Arc<EngineClient>,
        cfg: Arc<RollupConfig>,
        attributes: OpAttributesWithParent,
        is_attributes_derived: bool,
        payload_tx: Option<mpsc::Sender<OpExecutionPayloadEnvelope>>,
    ) -> Self {
        Self { engine, cfg, attributes, is_attributes_derived, payload_tx }
    }

    /// Starts the block building process by sending an initial `engine_forkchoiceUpdate` call with
    /// the payload attributes to build.
    ///
    /// ## Observed [PayloadStatusEnum] Variants
    /// The `engine_forkchoiceUpdate` payload statuses that this function observes are below. Any
    /// other [PayloadStatusEnum] variant is considered a failure.
    ///
    /// ### Success (`VALID`)
    /// If the build is successful, the [PayloadId] is returned for sealing and the external
    /// actor is notified of the successful forkchoice update.
    ///
    /// ### Failure (`INVALID`)
    /// If the forkchoice update fails, the external actor is notified of the failure.
    ///
    /// ### Syncing (`SYNCING`)
    /// If the EL is syncing, the payload attributes are buffered and the function returns early.
    /// This is a temporary state, and the function should be called again later.
    async fn start_build(
        &self,
        engine_client: &EngineClient,
        forkchoice: ForkchoiceState,
        attributes_envelope: OpAttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        debug!(
            target: "engine_builder",
            txs = attributes_envelope.inner().transactions.as_ref().map_or(0, |txs| txs.len()),
            "Starting new build job"
        );

        let forkchoice_version = EngineForkchoiceVersion::from_cfg(
            &self.cfg,
            attributes_envelope.inner().payload_attributes.timestamp,
        );
        debug!(target: "engine_builder", ?forkchoice_version, "Forkchoice version");
        let update = match forkchoice_version {
            EngineForkchoiceVersion::V3 => {
                engine_client
                    .fork_choice_updated_v3(forkchoice, Some(attributes_envelope.inner))
                    .await
            }
            EngineForkchoiceVersion::V2 => {
                engine_client
                    .fork_choice_updated_v2(forkchoice, Some(attributes_envelope.inner))
                    .await
            }
            EngineForkchoiceVersion::V1 => {
                engine_client
                    .fork_choice_updated_v1(
                        forkchoice,
                        Some(attributes_envelope.inner.payload_attributes),
                    )
                    .await
            }
        }
        .map_err(|e| {
            error!(target: "engine_builder", "Forkchoice update failed: {}", e);
            BuildTaskError::ForkchoiceUpdateFailed(e)
        })?;

        match update.payload_status.status {
            PayloadStatusEnum::Valid => {
                debug!(
                    target: "engine_builder",
                    unsafe_hash = forkchoice.head_block_hash.to_string(),
                    safe_hash = forkchoice.safe_block_hash.to_string(),
                    finalized_hash = forkchoice.finalized_block_hash.to_string(),
                    "Forkchoice update with attributes successful"
                );
            }
            PayloadStatusEnum::Invalid { validation_error } => {
                error!(target: "engine_builder", "Forkchoice update failed: {}", validation_error);
                return Err(BuildTaskError::ForkchoiceUpdateFailed(RpcError::local_usage_str(
                    &validation_error,
                )));
            }
            PayloadStatusEnum::Syncing => {
                warn!(target: "engine_builder", "Forkchoice update failed temporarily: EL is syncing");
                return Err(BuildTaskError::EngineSyncing);
            }
            s => {
                // Other codes are never returned by `engine_forkchoiceUpdate`
                return Err(BuildTaskError::UnexpectedPayloadStatus(s));
            }
        }

        // Fetch the payload ID from the FCU. If no payload ID was returned, something went wrong -
        // the block building job on the EL should have been initiated.
        update.payload_id.ok_or(BuildTaskError::MissingPayloadId)
    }

    /// Fetches the execution payload from the EL and imports it into the engine via
    /// `engine_newPayload`.
    ///
    /// ## Engine Method Selection
    /// The method used to fetch the payload from the EL is determined by the payload timestamp. The
    /// method used to import the payload into the engine is determined by the payload version.
    ///
    /// - `engine_getPayloadV2` is used for payloads with a timestamp before the Ecotone fork.
    /// - `engine_getPayloadV3` is used for payloads with a timestamp after the Ecotone fork.
    /// - `engine_getPayloadV4` is used for payloads with a timestamp after the Isthmus fork.
    async fn fetch_and_import_payload(
        &self,
        state: &mut EngineState,
        cfg: &RollupConfig,
        engine: &EngineClient,
        payload_id: PayloadId,
        payload_attrs: OpAttributesWithParent,
    ) -> Result<(OpExecutionPayloadEnvelope, L2BlockInfo), BuildTaskError> {
        let payload_timestamp = payload_attrs.inner().payload_attributes.timestamp;

        debug!(
            target: "engine_builder",
            payload_id = payload_id.to_string(),
            l2_time = payload_timestamp,
            "Inserting payload"
        );

        let get_payload_version = EngineGetPayloadVersion::from_cfg(cfg, payload_timestamp);
        let (payload_envelope, response) = match get_payload_version {
            EngineGetPayloadVersion::V4 => {
                let payload = engine.get_payload_v4(payload_id).await.map_err(|e| {
                    error!(target: "engine_builder", "Payload fetch failed: {e}");
                    BuildTaskError::GetPayloadFailed(e)
                })?;
                let response = engine
                    .new_payload_v4(
                        payload.execution_payload.clone(),
                        payload.parent_beacon_block_root,
                    )
                    .await
                    .map_err(|e| {
                        error!(target: "engine_builder", "Payload import failed: {e}");
                        BuildTaskError::NewPayloadFailed(e)
                    })?;

                (
                    OpExecutionPayloadEnvelope {
                        parent_beacon_block_root: Some(payload.parent_beacon_block_root),
                        payload: OpExecutionPayload::V4(payload.execution_payload),
                    },
                    response,
                )
            }
            EngineGetPayloadVersion::V3 => {
                let payload = engine.get_payload_v3(payload_id).await.map_err(|e| {
                    error!(target: "engine_builder", "Payload fetch failed: {e}");
                    BuildTaskError::GetPayloadFailed(e)
                })?;
                let response = engine
                    .new_payload_v3(
                        payload.execution_payload.clone(),
                        payload.parent_beacon_block_root,
                    )
                    .await
                    .map_err(|e| {
                        error!(target: "engine_builder", "Payload import failed: {e}");
                        BuildTaskError::NewPayloadFailed(e)
                    })?;

                (
                    OpExecutionPayloadEnvelope {
                        parent_beacon_block_root: Some(payload.parent_beacon_block_root),
                        payload: OpExecutionPayload::V3(payload.execution_payload),
                    },
                    response,
                )
            }
            EngineGetPayloadVersion::V2 => {
                let payload = engine.get_payload_v2(payload_id).await.map_err(|e| {
                    error!(target: "engine_builder", "Payload fetch failed: {e}");
                    BuildTaskError::GetPayloadFailed(e)
                })?;
                match payload.execution_payload {
                    ExecutionPayloadFieldV2::V2(payload) => {
                        let payload_input = ExecutionPayloadInputV2 {
                            execution_payload: payload.payload_inner.clone(),
                            withdrawals: Some(payload.withdrawals.clone()),
                        };
                        let response = engine.new_payload_v2(payload_input).await.map_err(|e| {
                            error!(target: "engine_builder", "Payload import failed: {e}");
                            BuildTaskError::NewPayloadFailed(e)
                        })?;

                        (
                            OpExecutionPayloadEnvelope {
                                parent_beacon_block_root: None,
                                payload: OpExecutionPayload::V2(payload),
                            },
                            response,
                        )
                    }
                    ExecutionPayloadFieldV2::V1(payload) => {
                        let response =
                            engine.new_payload_v1(payload.clone()).await.map_err(|e| {
                                error!(target: "engine_builder", "Payload import failed: {e}");
                                BuildTaskError::NewPayloadFailed(e)
                            })?;

                        (
                            OpExecutionPayloadEnvelope {
                                parent_beacon_block_root: None,
                                payload: OpExecutionPayload::V1(payload),
                            },
                            response,
                        )
                    }
                }
            }
        };

        match response.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {
                debug!(target: "engine_builder", "Payload import successful");

                Ok((
                    payload_envelope.clone(),
                    L2BlockInfo::from_payload_and_genesis(
                        payload_envelope.payload,
                        payload_attrs.inner().payload_attributes.parent_beacon_block_root,
                        &cfg.genesis,
                    )?,
                ))
            }
            PayloadStatusEnum::Invalid { validation_error } => {
                if payload_attrs.is_deposits_only() {
                    error!(target: "engine_builder", "Critical: Deposit-only payload import failed: {validation_error}");
                    Err(BuildTaskError::DepositOnlyPayloadFailed)
                } else if cfg.is_holocene_active(payload_attrs.inner().payload_attributes.timestamp)
                {
                    warn!(target: "engine_builder", "Payload import failed: {validation_error}");
                    warn!(target: "engine_builder", "Re-attempting payload import with deposits only.");
                    // HOLOCENE: Re-attempt payload import with deposits only
                    match Self::new(
                        self.engine.clone(),
                        self.cfg.clone(),
                        self.attributes.as_deposits_only(),
                        self.is_attributes_derived,
                        self.payload_tx.clone(),
                    )
                    .execute(state)
                    .await
                    {
                        Ok(_) => {
                            info!(target: "engine_builder", "Successfully imported deposits-only payload")
                        }
                        Err(_) => return Err(BuildTaskError::DepositOnlyPayloadReattemptFailed),
                    }
                    Err(BuildTaskError::HoloceneInvalidFlush)
                } else {
                    error!(target: "engine_builder", "Payload import failed: {validation_error}");
                    Err(BuildTaskError::NewPayloadFailed(RpcError::local_usage_str(
                        &validation_error,
                    )))
                }
            }
            s => {
                // Other codes are never returned by `engine_newPayload`
                Err(BuildTaskError::UnexpectedPayloadStatus(s))
            }
        }
    }
}

#[async_trait]
impl EngineTaskExt for BuildTask {
    async fn execute(&self, state: &mut EngineState) -> Result<(), EngineTaskError> {
        // Sanity check if the head is behind the finalized head. If it is, this is a critical
        // error.
        if state.unsafe_head().block_info.number < state.finalized_head().block_info.number {
            return Err(BuildTaskError::FinalizedAheadOfUnsafe(
                state.unsafe_head().block_info.number,
                state.finalized_head().block_info.number,
            )
            .into());
        }

        // Send the forkchoice update through the input, with the current engine state and the
        // payload attributes for the block building job.
        let mut forkchoice = state.create_forkchoice_state();
        forkchoice.head_block_hash = self.attributes.parent.block_info.hash;

        // Start the build by sending an FCU call with the current forkchoice and the input
        // payload attributes.
        let fcu_start_time = Instant::now();
        let payload_id =
            self.start_build(&self.engine, forkchoice, self.attributes.clone()).await?;
        let fcu_duration = fcu_start_time.elapsed();

        // Fetch the payload from the EL and import it into the engine.
        let block_import_start_time = Instant::now();
        let (new_payload, new_block_ref) = self
            .fetch_and_import_payload(
                state,
                &self.cfg,
                &self.engine,
                payload_id,
                self.attributes.clone(),
            )
            .await?;
        let block_import_duration = block_import_start_time.elapsed();

        // Update the engine state.
        state.set_unsafe_head(new_block_ref);
        state.set_cross_unsafe_head(new_block_ref);
        if self.is_attributes_derived {
            state.set_local_safe_head(new_block_ref);
            state.set_safe_head(new_block_ref);
        }

        // Send a FCU to canonicalize the imported block.
        ForkchoiceTask::new(Arc::clone(&self.engine)).execute(state).await?;

        // If a channel was provided, send the built payload envelope to it.
        if let Some(tx) = &self.payload_tx {
            tx.send(new_payload).await.map_err(BuildTaskError::MpscSend)?;
        }

        info!(
            target: "engine_builder",
            l2_number = new_block_ref.block_info.number,
            l2_time = new_block_ref.block_info.timestamp,
            fcu_duration = ?fcu_duration,
            block_import_duration = ?block_import_duration,
            "Built and imported new {} block",
            if self.is_attributes_derived { "safe" } else { "unsafe" },
        );

        // Update metrics.
        kona_macros::inc!(counter, Metrics::ENGINE_TASK_COUNT, Metrics::BUILD_TASK_LABEL);

        Ok(())
    }
}
