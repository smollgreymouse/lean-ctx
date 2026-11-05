// SPDX-License-Identifier: Apache-2.0
use super::*;

#[async_trait::async_trait]
impl crate::core::execution_lifecycle::ExecutionDriver for McpDriver<'_> {
    type Primitive = McpDispatched;
    type Processed = McpProcessedCall;
    type Output = CallToolResult;
    type Error = ErrorData;

    async fn identify_caller(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        self.server
            .ensure_session_presence()
            .await
            .map_err(|error| {
                ErrorData::internal_error(
                    format!("agent bus registration is required before tool execution: {error}"),
                    None,
                )
            })?;
        self.server.check_idle_expiry().await;
        Ok(crate::core::execution_lifecycle::StageDisposition::Applied)
    }

    async fn resolve_workspace(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        self.server.resolve_roots_once().await;
        let project = self.server.session.read().await.project_root.clone();
        crate::core::policy::runtime::bind_request_project(project.as_deref());
        Ok(crate::core::execution_lifecycle::StageDisposition::Applied)
    }

    async fn resolve_entitlements(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        if self.entitlements.is_none() {
            return Err(ErrorData::internal_error(
                "MCP entitlements were not resolved",
                None,
            ));
        }
        Ok(crate::core::execution_lifecycle::StageDisposition::Applied)
    }

    async fn apply_security_boundaries(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        let request = self.request.take().expect("MCP request consumed once");
        let entitlements = self
            .entitlements
            .take()
            .expect("MCP entitlements consumed once");
        self.prepared = Some(self.server.prepare_tool_call(request, entitlements).await?);
        Ok(crate::core::execution_lifecycle::StageDisposition::Applied)
    }

    async fn validate_cached_replay(
        &mut self,
        context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<Option<Self::Output>, Self::Error> {
        self.identify_caller(context).await?;
        self.resolve_workspace(context).await?;
        self.resolve_entitlements(context).await?;
        self.apply_security_boundaries(context).await?;
        // Replays revalidate admission only. Context gathering/planning already
        // belongs to the cached execution and must not run a second time.
        Ok(self
            .prepared
            .take()
            .expect("MCP replay admission consumed once")
            .into_replay_override())
    }

    async fn gather_context_strategy(
        &mut self,
        context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        let Some(PreparedCallResult::Ready(call)) = self.prepared.as_mut() else {
            return Ok(crate::core::execution_lifecycle::StageDisposition::Skipped(
                "admission returned a terminal result",
            ));
        };
        *self.server.task_envelope.write().await = Some(context.envelope.clone());
        call.minimal = call.config.minimal_overhead_effective();
        if !call.machine_readable {
            let (task, project_root) = {
                let session = self.server.session.read().await;
                (
                    session.task.as_ref().map(|task| task.description.clone()),
                    session.project_root.clone(),
                )
            };
            let cache_timeout =
                tokio::time::timeout(std::time::Duration::from_secs(5), self.server.cache.write())
                    .await;
            if let Ok(mut cache) = cache_timeout {
                call.auto_context = crate::tools::autonomy::session_lifecycle_pre_hook(
                    &self.server.autonomy,
                    &call.name,
                    &mut cache,
                    task.as_deref(),
                    project_root.as_deref(),
                    CrpMode::effective(),
                );
            } else {
                tracing::warn!("pre-dispatch: cache write-lock timeout (5s), skipping autonomy");
            }
        }
        Ok(crate::core::execution_lifecycle::StageDisposition::Applied)
    }

    async fn ask_autopilot(
        &mut self,
        context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        if !context.entitlements.autopilot {
            if let Some(PreparedCallResult::Ready(call)) = self.prepared.as_mut() {
                call.kernel_handoff =
                    crate::core::context_kernel::bridge::runtime::KernelPlanningHandoff::Suppressed;
            }
            return Ok(crate::core::execution_lifecycle::StageDisposition::Skipped(
                "not entitled",
            ));
        }
        let Some(PreparedCallResult::Ready(call)) = self.prepared.as_mut() else {
            return Ok(crate::core::execution_lifecycle::StageDisposition::Skipped(
                "admission returned a terminal result",
            ));
        };
        *self.server.task_envelope.write().await = Some(context.envelope.clone());
        call.decision_context = Some(context.clone());
        let (disposition, handoff) = kernel::plan(self.server, call, context).await;
        call.kernel_handoff = handoff;
        Ok(disposition)
    }

    async fn dispatch_primitive(
        &mut self,
        context: &crate::core::execution_lifecycle::TaskContext,
    ) -> Result<
        (
            Self::Primitive,
            crate::core::execution_lifecycle::StageDisposition,
        ),
        Self::Error,
    > {
        match self.prepared.take().expect("MCP admission consumed once") {
            PreparedCallResult::Terminal(result, reason) => Ok((
                McpDispatched::Terminal(result, reason),
                crate::core::execution_lifecycle::StageDisposition::Applied,
            )),
            PreparedCallResult::Cached(result) => Ok((
                McpDispatched::Terminal(result, "response cache hit"),
                crate::core::execution_lifecycle::StageDisposition::Applied,
            )),
            PreparedCallResult::Ready(call) => {
                // Context gathering can await locks/services after admission.
                // Recheck current tool and egress rules before any primitive,
                // including the inner tool carried by ctx_call, is dispatched.
                let (guard_name, guard_args) =
                    crate::tools::registered::ctx_call::resolve(&call.name, call.args.as_ref())?;
                if let Some(denied) = LeanCtxServer::guard_role_and_policy(&guard_name)
                    .or_else(|| LeanCtxServer::guard_egress(&guard_name, guard_args.as_ref()))
                {
                    return Ok((
                        McpDispatched::Terminal(denied, "policy changed before dispatch"),
                        crate::core::execution_lifecycle::StageDisposition::Applied,
                    ));
                }
                let one_shot_shell_approved = if guard_name == "ctx_shell" {
                    guard_args
                        .as_ref()
                        .and_then(|args| args.get("command"))
                        .and_then(Value::as_str)
                        .and_then(crate::core::shell_allowlist::one_shot_approval_required)
                        .is_some_and(|blocked| {
                            tracing::info!(
                                reason = %blocked,
                                "requesting one-shot host approval for shell allowlist miss"
                            );
                            true
                        })
                } else {
                    false
                };

                let one_shot_shell_approved = if one_shot_shell_approved {
                    let command = guard_args
                        .as_ref()
                        .and_then(|args| args.get("command"))
                        .and_then(Value::as_str)
                        .expect("shell approval candidate has command");
                    self.server
                        .request_shell_one_shot_approval(
                            command,
                            Value::Object(guard_args.clone().unwrap_or_default()),
                        )
                        .await
                } else {
                    false
                };

                if crate::server::native_receipts::requested(&call.name, call.args.as_ref()) {
                    self.native_receipt_capture = self
                        .server
                        .native_receipt_authority
                        .as_ref()
                        .map(|authority| {
                            Arc::new(crate::server::native_receipts::NativeReceiptCapture::new(
                                authority.clone(),
                                context.envelope.clone(),
                            ))
                        });
                }
                let dispatch_start = std::time::Instant::now();
                let dispatch = super::super::pipeline::dispatch_primitive(
                    self.server,
                    &call.name,
                    call.args.as_ref(),
                    call.minimal,
                    &call.args_fp,
                );
                let primitive = crate::server::tool_trait::SHELL_ONE_SHOT_APPROVED
                    .scope(
                        one_shot_shell_approved,
                        crate::server::native_receipts::NATIVE_RECEIPT_CAPTURE.scope(
                            self.native_receipt_capture.clone(),
                            crate::core::context_kernel::bridge::runtime::KERNEL_PLANNING_HANDOFF
                                .scope(call.kernel_handoff.clone(), dispatch),
                        ),
                    )
                    .await;
                self.server.record_tool_usage(
                    &call.name,
                    dispatch_start,
                    primitive
                        .as_ref()
                        .is_ok_and(super::super::pipeline::McpPrimitive::succeeded),
                );
                let primitive = primitive?;
                Ok((
                    McpDispatched::Raw { call, primitive },
                    crate::core::execution_lifecycle::StageDisposition::Applied,
                ))
            }
        }
    }

    async fn reversible_post_process(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
        primitive: Self::Primitive,
    ) -> Result<
        (
            Self::Processed,
            crate::core::execution_lifecycle::StageDisposition,
        ),
        Self::Error,
    > {
        match primitive {
            McpDispatched::Terminal(result, reason) => Ok((
                McpProcessedCall {
                    result,
                    pipeline: None,
                    cache_key: None,
                    skip_reason: Some(reason),
                },
                crate::core::execution_lifecycle::StageDisposition::Skipped(reason),
            )),
            McpDispatched::Raw { call, primitive } => {
                let disposition = match &primitive {
                    super::super::pipeline::McpPrimitive::Raw(_) => {
                        crate::core::execution_lifecycle::StageDisposition::Applied
                    }
                    super::super::pipeline::McpPrimitive::Terminal(_, reason) => {
                        crate::core::execution_lifecycle::StageDisposition::Skipped(reason)
                    }
                };
                let mut processed = super::super::pipeline::reversible_post_process(
                    self.server,
                    &call.name,
                    call.args.as_ref(),
                    call.minimal,
                    call.config,
                    call.machine_readable,
                    call.auto_context,
                    call.throttle_warning,
                    call.decision_context,
                    primitive,
                )
                .await?;
                if self.native_receipt_capture.is_some() {
                    processed.freeze_receipt_delivery();
                }
                Ok((
                    McpProcessedCall {
                        result: processed.result.clone(),
                        pipeline: Some(processed),
                        cache_key: call.cache_key,
                        skip_reason: None,
                    },
                    disposition,
                ))
            }
        }
    }

    async fn record_context_ir(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
        processed: &Self::Processed,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        Ok(match processed.pipeline.as_ref() {
            Some(pipeline) => {
                super::super::pipeline::record_context_ir(self.server, pipeline).await
            }
            None => crate::core::execution_lifecycle::StageDisposition::Skipped(
                processed.skip_reason.unwrap_or("terminal result"),
            ),
        })
    }

    async fn record_ledger(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
        processed: &Self::Processed,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        if let Some(capture) = &self.native_receipt_capture {
            self.native_receipt_metadata = Some(
                capture
                    .publish(&processed.result)
                    .map_err(|code| ErrorData::internal_error(code, None))?,
            );
        }
        Ok(match processed.pipeline.as_ref() {
            Some(pipeline) => super::super::pipeline::record_ledger(self.server, pipeline).await?,
            None => crate::core::execution_lifecycle::StageDisposition::Skipped(
                processed.skip_reason.unwrap_or("terminal result"),
            ),
        })
    }

    async fn checkpoint(
        &mut self,
        _context: &crate::core::execution_lifecycle::TaskContext,
        output: &mut Self::Output,
    ) -> Result<crate::core::execution_lifecycle::StageDisposition, Self::Error> {
        let disposition =
            super::super::pipeline::record_checkpoint(self.server, self.checkpoint.take(), output)
                .await;
        if let Some(key) = self.pending_cache_key.take() {
            cache_call_result(global_response_cache(), key, output);
        }
        Ok(disposition)
    }

    fn output_from_processed(&mut self, mut processed: Self::Processed) -> Self::Output {
        if let Some(pipeline) = processed.pipeline.as_mut() {
            self.checkpoint = pipeline.checkpoint.take();
        }
        self.pending_cache_key = processed.cache_key;
        if let Some(receipt) = self.native_receipt_metadata.take() {
            processed
                .result
                .meta
                .get_or_insert_with(Meta::new)
                .0
                .insert("canonical_receipt".into(), receipt);
        }
        processed.result
    }

    fn observe(
        &self,
        result: &Result<
            Self::Output,
            crate::core::execution_lifecycle::LifecycleRunError<Self::Error>,
        >,
    ) -> crate::core::execution_lifecycle::CompletionObservation {
        let (output_tokens, success) = result.as_ref().map_or((0, false), |response| {
            (
                u64::try_from(format!("{response:?}").len() / 4).unwrap_or(u64::MAX),
                response.is_error != Some(true),
            )
        });
        crate::core::execution_lifecycle::CompletionObservation::tool_result(
            self.input_tokens,
            output_tokens,
            "mcp-tool",
            success,
        )
    }
}
