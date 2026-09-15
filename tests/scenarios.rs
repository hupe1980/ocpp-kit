//! Certification-profile scenarios: every Core-profile action driven over a real socket.
//!
//! `tests/schema_conformance.rs` proves the *types* match the schemas, for every action of every
//! version. That is a different claim from "this action works": it says nothing about whether the
//! action name dispatches, whether the request reaches a handler, or whether the response the
//! handler builds survives the round trip back to the caller as the right type.
//!
//! This file makes that claim, for the actions OCPP 2.0.1 Part 5 lists in the **Core** profile —
//! which is what `cargo xtask coverage --profile core` measures. Each exchange is a real
//! `CALL`/`CALLRESULT` over loopback TCP, through the WebSocket layer, the engine, and the
//! generated dispatch unions in both directions.
//!
//! It is still not certification ([QUALITY.md]); it is the traceability that makes certification
//! tractable, and it is the difference between an action being *typed* and an action being
//! *driven*.

// A scenario test is a transcript of an exchange, and splitting one into helpers to satisfy a
// line count would make it harder to read against the specification, which is the only reason
// to have it.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{
    AcceptEveryStation, BasicAuthPassword, BoxFuture, Csms, CsmsHandle, Ctx, Handler,
    SecurityProfile, Station,
};
use ocpp_kit::types::{DateTime, Identity};
use ocpp_kit::{RawValue, Version, v2_1};
use tokio::net::TcpListener;

const STATION: &str = "CS-CORE-01";

// ---------------------------------------------------------------------------
// The station side: answers everything the CSMS may ask of a Core-profile station
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CoreStation {
    handled: AtomicUsize,
}

impl Handler for CoreStation {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            use v2_1::CsmsRequest as Req;

            let action = v2_1::Action::from_wire(&request.action)
                .ok_or_else(|| CallError::not_implemented(&request.action))?;
            let decoded = Req::decode(action, &request.payload, ctx.decode_options())?;
            self.handled.fetch_add(1, Ordering::SeqCst);

            match decoded {
                // --- B: Provisioning -------------------------------------------------
                Req::GetBaseReport(_) => ctx.reply(&v2_1::GetBaseReportResponse::new(
                    v2_1::GenericDeviceModelStatus::Accepted,
                )),
                Req::GetReport(_) => ctx.reply(&v2_1::GetReportResponse::new(
                    v2_1::GenericDeviceModelStatus::Accepted,
                )),
                Req::GetVariables(_) => {
                    ctx.reply(&v2_1::GetVariablesResponse::new(alloc_get_variable_result()))
                }
                Req::SetVariables(_) => ctx.reply(&v2_1::SetVariablesResponse::new(vec![
                    v2_1::SetVariableResult::new(
                        v2_1::SetVariableStatus::Accepted,
                        v2_1::Component::new("OCPPCommCtrlr"),
                        v2_1::Variable::new("HeartbeatInterval"),
                    ),
                ])),
                Req::SetNetworkProfile(_) => ctx.reply(&v2_1::SetNetworkProfileResponse::new(
                    v2_1::SetNetworkProfileStatus::Accepted,
                )),
                Req::Reset(_) => ctx.reply(&v2_1::ResetResponse::new(v2_1::ResetStatus::Accepted)),
                Req::TriggerMessage(_) => ctx.reply(&v2_1::TriggerMessageResponse::new(
                    v2_1::TriggerMessageStatus::Accepted,
                )),

                // --- G: Availability -------------------------------------------------
                Req::ChangeAvailability(_) => ctx.reply(&v2_1::ChangeAvailabilityResponse::new(
                    v2_1::ChangeAvailabilityStatus::Accepted,
                )),

                // --- C/E: Authorization and transactions ------------------------------
                Req::ClearCache(_) => ctx.reply(&v2_1::ClearCacheResponse::new(
                    v2_1::ClearCacheStatus::Accepted,
                )),
                Req::GetTransactionStatus(_) => {
                    // `messagesInQueue` is what `Engine::queued()` answers in a real station.
                    ctx.reply(&v2_1::GetTransactionStatusResponse::new(false))
                }
                Req::RequestStartTransaction(_) => {
                    ctx.reply(&v2_1::RequestStartTransactionResponse::new(
                        v2_1::RequestStartStopStatus::Accepted,
                    ))
                }
                Req::RequestStopTransaction(_) => {
                    ctx.reply(&v2_1::RequestStopTransactionResponse::new(
                        v2_1::RequestStartStopStatus::Accepted,
                    ))
                }
                Req::UnlockConnector(_) => ctx.reply(&v2_1::UnlockConnectorResponse::new(
                    v2_1::UnlockStatus::Unlocked,
                )),

                // --- M: Certificate management ---------------------------------------
                Req::InstallCertificate(_) => ctx.reply(&v2_1::InstallCertificateResponse::new(
                    v2_1::InstallCertificateStatus::Accepted,
                )),
                Req::GetInstalledCertificateIds(_) => {
                    ctx.reply(&v2_1::GetInstalledCertificateIdsResponse::new(
                        v2_1::GetInstalledCertificateStatus::Accepted,
                    ))
                }
                Req::DeleteCertificate(_) => ctx.reply(&v2_1::DeleteCertificateResponse::new(
                    v2_1::DeleteCertificateStatus::Accepted,
                )),

                // --- N: Diagnostics ---------------------------------------------------
                Req::GetLog(_) => ctx.reply(&v2_1::GetLogResponse::new(v2_1::LogStatus::Accepted)),
                Req::CustomerInformation(_) => ctx.reply(&v2_1::CustomerInformationResponse::new(
                    v2_1::CustomerInformationStatus::Accepted,
                )),

                // --- L: Firmware management -------------------------------------------
                Req::UpdateFirmware(_) => ctx.reply(&v2_1::UpdateFirmwareResponse::new(
                    v2_1::UpdateFirmwareStatus::Accepted,
                )),
                Req::PublishFirmware(_) => ctx.reply(&v2_1::PublishFirmwareResponse::new(
                    v2_1::GenericStatus::Accepted,
                )),
                Req::UnpublishFirmware(_) => ctx.reply(&v2_1::UnpublishFirmwareResponse::new(
                    v2_1::UnpublishFirmwareStatus::Unpublished,
                )),

                // --- A: Advanced Security ---------------------------------------------
                Req::CertificateSigned(_) => ctx.reply(&v2_1::CertificateSignedResponse::new(
                    v2_1::CertificateSignedStatus::Accepted,
                )),

                // --- D: Local authorization list --------------------------------------
                Req::SendLocalList(_) => ctx.reply(&v2_1::SendLocalListResponse::new(
                    v2_1::SendLocalListStatus::Accepted,
                )),
                Req::GetLocalListVersion(_) => {
                    ctx.reply(&v2_1::GetLocalListVersionResponse::new(3))
                }

                // --- K: Smart charging -------------------------------------------------
                Req::GetChargingProfiles(_) => ctx.reply(&v2_1::GetChargingProfilesResponse::new(
                    v2_1::GetChargingProfileStatus::Accepted,
                )),
                Req::ClearChargingProfile(_) => {
                    ctx.reply(&v2_1::ClearChargingProfileResponse::new(
                        v2_1::ClearChargingProfileStatus::Accepted,
                    ))
                }
                Req::GetCompositeSchedule(_) => ctx.reply(
                    &v2_1::GetCompositeScheduleResponse::new(v2_1::GenericStatus::Accepted),
                ),
                Req::SetChargingProfile(_) => ctx.reply(&v2_1::SetChargingProfileResponse::new(
                    v2_1::ChargingProfileStatus::Accepted,
                )),

                // --- N: Advanced device management (monitoring) ------------------------
                Req::GetMonitoringReport(_) => ctx.reply(&v2_1::GetMonitoringReportResponse::new(
                    v2_1::GenericDeviceModelStatus::Accepted,
                )),
                Req::SetMonitoringBase(_) => ctx.reply(&v2_1::SetMonitoringBaseResponse::new(
                    v2_1::GenericDeviceModelStatus::Accepted,
                )),
                Req::SetMonitoringLevel(_) => ctx.reply(&v2_1::SetMonitoringLevelResponse::new(
                    v2_1::GenericStatus::Accepted,
                )),
                Req::SetVariableMonitoring(_) => {
                    ctx.reply(&v2_1::SetVariableMonitoringResponse::new(vec![
                        v2_1::SetMonitoringResult::new(
                            v2_1::SetMonitoringStatus::Accepted,
                            v2_1::Monitor::UpperThreshold,
                            v2_1::Component::new("OCPPCommCtrlr"),
                            v2_1::Variable::new("HeartbeatInterval"),
                            5,
                        ),
                    ]))
                }
                Req::ClearVariableMonitoring(_) => {
                    ctx.reply(&v2_1::ClearVariableMonitoringResponse::new(vec![
                        v2_1::ClearMonitoringResult::new(v2_1::ClearMonitoringStatus::Accepted, 1),
                    ]))
                }

                // --- O: Display messages -----------------------------------------------
                Req::GetDisplayMessages(_) => ctx.reply(&v2_1::GetDisplayMessagesResponse::new(
                    v2_1::GetDisplayMessagesStatus::Accepted,
                )),
                Req::ClearDisplayMessage(_) => ctx.reply(&v2_1::ClearDisplayMessageResponse::new(
                    v2_1::ClearMessageStatus::Accepted,
                )),
                Req::CostUpdated(_) => ctx.reply(&v2_1::CostUpdatedResponse::new()),

                // --- H: Reservation -----------------------------------------------------
                Req::ReserveNow(_) => ctx.reply(&v2_1::ReserveNowResponse::new(
                    v2_1::ReserveNowStatus::Accepted,
                )),
                Req::CancelReservation(_) => ctx.reply(&v2_1::CancelReservationResponse::new(
                    v2_1::CancelReservationStatus::Accepted,
                )),

                // --- P: Data transfer -------------------------------------------------
                Req::DataTransfer(_) => ctx.reply(&v2_1::DataTransferResponse::new(
                    v2_1::DataTransferStatus::Accepted,
                )),

                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

fn alloc_get_variable_result() -> Vec<v2_1::GetVariableResult> {
    vec![v2_1::GetVariableResult::new(
        v2_1::GetVariableStatus::Accepted,
        v2_1::Component::new("OCPPCommCtrlr"),
        v2_1::Variable::new("HeartbeatInterval"),
    )]
}

// ---------------------------------------------------------------------------
// The CSMS side: answers everything a Core-profile station reports
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CoreCsms {
    handled: AtomicUsize,
}

impl Handler for CoreCsms {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            use v2_1::CsRequest as Req;

            let action = v2_1::Action::from_wire(&request.action)
                .ok_or_else(|| CallError::not_implemented(&request.action))?;
            let decoded = Req::decode(action, &request.payload, ctx.decode_options())?;
            self.handled.fetch_add(1, Ordering::SeqCst);

            match decoded {
                Req::BootNotification(_) => ctx.reply(&v2_1::BootNotificationResponse::new(
                    DateTime::now(),
                    300,
                    v2_1::RegistrationStatus::Accepted,
                )),
                Req::Heartbeat(_) => ctx.reply(&v2_1::HeartbeatResponse::new(DateTime::now())),
                Req::StatusNotification(_) => ctx.reply(&v2_1::StatusNotificationResponse::new()),
                Req::Authorize(_) => ctx.reply(&v2_1::AuthorizeResponse::new(
                    v2_1::IdTokenInfo::new(v2_1::AuthorizationStatus::Accepted),
                )),
                Req::MeterValues(_) => ctx.reply(&v2_1::MeterValuesResponse::new()),
                Req::NotifyReport(_) => ctx.reply(&v2_1::NotifyReportResponse::new()),
                Req::SecurityEventNotification(_) => {
                    ctx.reply(&v2_1::SecurityEventNotificationResponse::new())
                }
                Req::LogStatusNotification(_) => {
                    ctx.reply(&v2_1::LogStatusNotificationResponse::new())
                }
                Req::NotifyCustomerInformation(_) => {
                    ctx.reply(&v2_1::NotifyCustomerInformationResponse::new())
                }
                Req::FirmwareStatusNotification(_) => {
                    ctx.reply(&v2_1::FirmwareStatusNotificationResponse::new())
                }
                Req::PublishFirmwareStatusNotification(_) => {
                    ctx.reply(&v2_1::PublishFirmwareStatusNotificationResponse::new())
                }
                Req::SignCertificate(_) => ctx.reply(&v2_1::SignCertificateResponse::new(
                    v2_1::GenericStatus::Accepted,
                )),
                Req::ReportChargingProfiles(_) => {
                    ctx.reply(&v2_1::ReportChargingProfilesResponse::new())
                }
                Req::NotifyChargingLimit(_) => ctx.reply(&v2_1::NotifyChargingLimitResponse::new()),
                Req::ClearedChargingLimit(_) => {
                    ctx.reply(&v2_1::ClearedChargingLimitResponse::new())
                }
                Req::NotifyMonitoringReport(_) => {
                    ctx.reply(&v2_1::NotifyMonitoringReportResponse::new())
                }
                Req::NotifyEvent(_) => ctx.reply(&v2_1::NotifyEventResponse::new()),
                Req::NotifyDisplayMessages(_) => {
                    ctx.reply(&v2_1::NotifyDisplayMessagesResponse::new())
                }
                Req::ReservationStatusUpdate(_) => {
                    ctx.reply(&v2_1::ReservationStatusUpdateResponse::new())
                }
                Req::Get15118EVCertificate(_) => {
                    ctx.reply(&v2_1::Get15118EVCertificateResponse::new(
                        v2_1::Iso15118EVCertificateStatus::Accepted,
                        "exi-response",
                    ))
                }
                Req::GetCertificateStatus(_) => {
                    ctx.reply(&v2_1::GetCertificateStatusResponse::new(
                        v2_1::GetCertificateStatusEnum::Accepted,
                    ))
                }
                Req::NotifyEVChargingNeeds(_) => {
                    ctx.reply(&v2_1::NotifyEVChargingNeedsResponse::new(
                        v2_1::NotifyEVChargingNeedsStatus::Accepted,
                    ))
                }
                Req::NotifyEVChargingSchedule(_) => ctx.reply(
                    &v2_1::NotifyEVChargingScheduleResponse::new(v2_1::GenericStatus::Accepted),
                ),
                Req::DataTransfer(_) => ctx.reply(&v2_1::DataTransferResponse::new(
                    v2_1::DataTransferStatus::Accepted,
                )),
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct ArcHandler<H>(Arc<H>);

impl<H: Handler> Handler for ArcHandler<H> {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        self.0.on_request(ctx, request)
    }
}

/// A connected station and CSMS, with the boot exchange already done.
///
/// Returns both handles: the engine enforces which peer may originate each action, so a
/// station-originated call has to go out through the *station's* handle. Trying it from the
/// CSMS side fails with `this peer may not originate …`, which is the rule working.
async fn connected() -> (
    CsmsHandle,
    Arc<CoreCsms>,
    Arc<CoreStation>,
    ocpp_kit::transport::Handle,
    Identity,
) {
    let csms_handler = Arc::new(CoreCsms::default());
    let station_handler = Arc::new(CoreStation::default());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let csms = Csms::builder()
        .bind(address)
        .versions([Version::V2_1])
        .authenticate(AcceptEveryStation)
        .handler(ArcHandler(csms_handler.clone()))
        .ping_interval(None)
        .build()
        .unwrap();
    let csms_handle = csms.handle();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    let station = Station::builder()
        .identity(STATION)
        .unwrap()
        .url(format!("ws://{address}/ocpp"))
        .versions([Version::V2_1])
        // The builder refuses profile 1 without a password, which is exactly the validation
        // `the_documented_security_profile_configurations_build` covers — so a scenario test
        // has to configure a real one rather than route around it.
        .security_profile(SecurityProfile::BasicAuth)
        .password(BasicAuthPassword::utf8("0123456789abcdef").unwrap())
        .handler(ArcHandler(station_handler.clone()))
        .ping_interval(None)
        .build()
        .unwrap();
    let station_handle = station.spawn().unwrap();

    // The boot gate holds everything else back until the CSMS accepts the station, so this is
    // not merely setup: without it every call below would sit in the queue.
    let boot = station_handle
        .call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        ))
        .await
        .expect("the boot exchange");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Accepted);

    let identity = Identity::new(STATION).unwrap();
    // The station's session has to be registered before the CSMS can call it back.
    for _ in 0..50 {
        if csms_handle.session(&identity).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    (
        csms_handle,
        csms_handler,
        station_handler,
        station_handle,
        identity,
    )
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Every Core-profile action the CSMS originates, driven over a real socket.
///
/// Each one asserts the *typed response*, so a dispatch arm that decoded the wrong payload or a
/// response that failed to serialize is a failure here rather than a surprise in the field.
#[tokio::test]
async fn every_core_profile_action_the_csms_originates_round_trips() {
    let (csms, _csms_handler, station, _station_handle, id) = connected().await;

    // --- B: Provisioning ---------------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::GetBaseReportRequest::new(1, v2_1::ReportBase::FullInventory)
        )
        .await
        .unwrap()
        .status,
        v2_1::GenericDeviceModelStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::GetReportRequest::new(2))
            .await
            .unwrap()
            .status,
        v2_1::GenericDeviceModelStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::GetVariablesRequest::new(vec![v2_1::GetVariableData::new(
                v2_1::Component::new("OCPPCommCtrlr"),
                v2_1::Variable::new("HeartbeatInterval"),
            )]),
        )
        .await
        .unwrap()
        .get_variable_result
        .len(),
        1,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::SetVariablesRequest::new(vec![v2_1::SetVariableData::new(
                "600",
                v2_1::Component::new("OCPPCommCtrlr"),
                v2_1::Variable::new("HeartbeatInterval"),
            )]),
        )
        .await
        .unwrap()
        .set_variable_result[0]
            .attribute_status,
        v2_1::SetVariableStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::SetNetworkProfileRequest::new(
                1,
                v2_1::NetworkConnectionProfile::new(
                    v2_1::OCPPInterface::Wired0,
                    v2_1::OCPPTransport::JSON,
                    30,
                    "wss://csms.example.com/ocpp",
                    2,
                ),
            ),
        )
        .await
        .unwrap()
        .status,
        v2_1::SetNetworkProfileStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::ResetRequest::new(v2_1::ResetEnum::OnIdle))
            .await
            .unwrap()
            .status,
        v2_1::ResetStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::TriggerMessageRequest::new(v2_1::MessageTrigger::StatusNotification),
        )
        .await
        .unwrap()
        .status,
        v2_1::TriggerMessageStatus::Accepted,
    );

    // --- G: Availability ---------------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::ChangeAvailabilityRequest::new(v2_1::OperationalStatus::Inoperative),
        )
        .await
        .unwrap()
        .status,
        v2_1::ChangeAvailabilityStatus::Accepted,
    );

    // --- C / E: Authorization and transactions -----------------------------
    assert_eq!(
        csms.call(&id, v2_1::ClearCacheRequest::new())
            .await
            .unwrap()
            .status,
        v2_1::ClearCacheStatus::Accepted,
    );
    assert!(
        !csms
            .call(&id, v2_1::GetTransactionStatusRequest::new())
            .await
            .unwrap()
            .messages_in_queue,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::RequestStartTransactionRequest::new(v2_1::IdToken::new("TAG-1", "ISO14443"), 7,),
        )
        .await
        .unwrap()
        .status,
        v2_1::RequestStartStopStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::RequestStopTransactionRequest::new("tx-1"))
            .await
            .unwrap()
            .status,
        v2_1::RequestStartStopStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::UnlockConnectorRequest::new(1, 1))
            .await
            .unwrap()
            .status,
        v2_1::UnlockStatus::Unlocked,
    );

    // --- M: Certificate management ------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::InstallCertificateRequest::new(
                v2_1::InstallCertificateUse::CSMSRootCertificate,
                "-----BEGIN CERTIFICATE-----",
            ),
        )
        .await
        .unwrap()
        .status,
        v2_1::InstallCertificateStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::GetInstalledCertificateIdsRequest::new())
            .await
            .unwrap()
            .status,
        v2_1::GetInstalledCertificateStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::DeleteCertificateRequest::new(v2_1::CertificateHashData::new(
                v2_1::HashAlgorithm::SHA256,
                "aa",
                "bb",
                "01",
            )),
        )
        .await
        .unwrap()
        .status,
        v2_1::DeleteCertificateStatus::Accepted,
    );

    // --- N: Diagnostics -----------------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::GetLogRequest::new(
                v2_1::LogParameters::new("ftp://logs.example.com/upload"),
                v2_1::Log::DiagnosticsLog,
                3,
            ),
        )
        .await
        .unwrap()
        .status,
        v2_1::LogStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::CustomerInformationRequest::new(4, true, false))
            .await
            .unwrap()
            .status,
        v2_1::CustomerInformationStatus::Accepted,
    );

    // --- L: Firmware management ---------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::UpdateFirmwareRequest::new(
                5,
                v2_1::Firmware::new("https://example.com/fw.bin", DateTime::now()),
            ),
        )
        .await
        .unwrap()
        .status,
        v2_1::UpdateFirmwareStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::PublishFirmwareRequest::new("https://example.com/fw.bin", "0badc0de", 6),
        )
        .await
        .unwrap()
        .status,
        v2_1::GenericStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::UnpublishFirmwareRequest::new("0badc0de"))
            .await
            .unwrap()
            .status,
        v2_1::UnpublishFirmwareStatus::Unpublished,
    );

    // --- P: Data transfer ----------------------------------------------------
    assert_eq!(
        csms.call(&id, v2_1::DataTransferRequest::new("ACME"))
            .await
            .unwrap()
            .status,
        v2_1::DataTransferStatus::Accepted,
    );

    assert_eq!(
        station.handled.load(Ordering::SeqCst),
        22,
        "every call above has to have reached the station's handler, and be decoded there",
    );
}

/// Every Core-profile action the station originates, driven over a real socket.
#[tokio::test]
async fn every_core_profile_action_the_station_originates_round_trips() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let csms_handler = Arc::new(CoreCsms::default());
    let csms = Csms::builder()
        .bind(address)
        .versions([Version::V2_1])
        .authenticate(AcceptEveryStation)
        .handler(ArcHandler(csms_handler.clone()))
        .ping_interval(None)
        .build()
        .unwrap();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    let station = Station::builder()
        .identity(STATION)
        .unwrap()
        .url(format!("ws://{address}/ocpp"))
        .versions([Version::V2_1])
        .security_profile(SecurityProfile::BasicAuth)
        .password(BasicAuthPassword::utf8("0123456789abcdef").unwrap())
        .handler(CoreStation::default())
        .ping_interval(None)
        .build()
        .unwrap();
    let handle = station.spawn().unwrap();

    handle
        .call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        ))
        .await
        .unwrap();

    handle.call(v2_1::HeartbeatRequest::new()).await.unwrap();
    handle
        .call(v2_1::StatusNotificationRequest::new(
            DateTime::now(),
            v2_1::ConnectorStatus::Available,
            1,
            1,
        ))
        .await
        .unwrap();
    assert_eq!(
        handle
            .call(v2_1::AuthorizeRequest::new(v2_1::IdToken::new(
                "TAG-1", "ISO14443"
            )))
            .await
            .unwrap()
            .id_token_info
            .status,
        v2_1::AuthorizationStatus::Accepted,
    );
    handle
        .call(v2_1::MeterValuesRequest::new(
            1,
            vec![v2_1::MeterValue::new(
                vec![v2_1::SampledValue::new(ocpp_kit::decimal!(2935.600))],
                DateTime::now(),
            )],
        ))
        .await
        .unwrap();
    handle
        .call(v2_1::NotifyReportRequest::new(1, DateTime::now(), 0))
        .await
        .unwrap();
    handle
        .call(v2_1::SecurityEventNotificationRequest::new(
            "TamperDetectionActivated",
            DateTime::now(),
        ))
        .await
        .unwrap();
    handle
        .call(v2_1::LogStatusNotificationRequest::new(
            v2_1::UploadLogStatus::Uploaded,
        ))
        .await
        .unwrap();
    handle
        .call(v2_1::NotifyCustomerInformationRequest::new(
            "some-customer-data",
            0,
            DateTime::now(),
            4,
        ))
        .await
        .unwrap();
    handle
        .call(v2_1::FirmwareStatusNotificationRequest::new(
            v2_1::FirmwareStatus::Installed,
        ))
        .await
        .unwrap();
    handle
        .call(v2_1::PublishFirmwareStatusNotificationRequest::new(
            v2_1::PublishFirmwareStatus::Published,
        ))
        .await
        .unwrap();

    assert_eq!(
        csms_handler.handled.load(Ordering::SeqCst),
        11,
        "the boot plus every notification above",
    );
}

/// The seven certification profiles beyond Core, driven the same way.
///
/// Together with the two tests above this reaches every action OCPP 2.0.1 Part 5 names in any
/// profile — `cargo xtask coverage --profile <name>` reports each one.
#[tokio::test]
async fn every_remaining_certification_profile_action_round_trips() {
    let (csms, csms_handler, station, station_handle, id) = connected().await;

    // --- A: Advanced Security (CSMS -> station) -----------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::CertificateSignedRequest::new("-----BEGIN CERTIFICATE-----")
        )
        .await
        .unwrap()
        .status,
        v2_1::CertificateSignedStatus::Accepted,
    );

    // --- D: Local authorization list ----------------------------------------
    assert_eq!(
        csms.call(&id, v2_1::SendLocalListRequest::new(3, v2_1::Update::Full),)
            .await
            .unwrap()
            .status,
        v2_1::SendLocalListStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::GetLocalListVersionRequest::new())
            .await
            .unwrap()
            .version_number,
        3,
    );

    // --- K: Smart charging ---------------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::GetChargingProfilesRequest::new(1, v2_1::ChargingProfileCriterion::new()),
        )
        .await
        .unwrap()
        .status,
        v2_1::GetChargingProfileStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::ClearChargingProfileRequest::new())
            .await
            .unwrap()
            .status,
        v2_1::ClearChargingProfileStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::GetCompositeScheduleRequest::new(3600, 1))
            .await
            .unwrap()
            .status,
        v2_1::GenericStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::SetChargingProfileRequest::new(1, sample_profile())
        )
        .await
        .unwrap()
        .status,
        v2_1::ChargingProfileStatus::Accepted,
    );

    // --- N: Advanced device management --------------------------------------
    assert_eq!(
        csms.call(&id, v2_1::GetMonitoringReportRequest::new(9))
            .await
            .unwrap()
            .status,
        v2_1::GenericDeviceModelStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::SetMonitoringBaseRequest::new(v2_1::MonitoringBase::All),
        )
        .await
        .unwrap()
        .status,
        v2_1::GenericDeviceModelStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::SetMonitoringLevelRequest::new(5))
            .await
            .unwrap()
            .status,
        v2_1::GenericStatus::Accepted,
    );
    assert_eq!(
        csms.call(
            &id,
            v2_1::SetVariableMonitoringRequest::new(vec![v2_1::SetMonitoringData::new(
                ocpp_kit::decimal!(80),
                v2_1::Monitor::UpperThreshold,
                5,
                v2_1::Component::new("OCPPCommCtrlr"),
                v2_1::Variable::new("HeartbeatInterval"),
            )]),
        )
        .await
        .unwrap()
        .set_monitoring_result
        .len(),
        1,
    );
    assert_eq!(
        csms.call(&id, v2_1::ClearVariableMonitoringRequest::new(vec![1]))
            .await
            .unwrap()
            .clear_monitoring_result
            .len(),
        1,
    );

    // --- O: Display messages -------------------------------------------------
    assert_eq!(
        csms.call(&id, v2_1::GetDisplayMessagesRequest::new(11))
            .await
            .unwrap()
            .status,
        v2_1::GetDisplayMessagesStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::ClearDisplayMessageRequest::new(1))
            .await
            .unwrap()
            .status,
        v2_1::ClearMessageStatus::Accepted,
    );
    csms.call(
        &id,
        v2_1::CostUpdatedRequest::new(ocpp_kit::decimal!(4.25), "tx-1"),
    )
    .await
    .unwrap();

    // --- H: Reservation -------------------------------------------------------
    assert_eq!(
        csms.call(
            &id,
            v2_1::ReserveNowRequest::new(
                1,
                DateTime::now(),
                v2_1::IdToken::new("TAG-1", "ISO14443"),
            ),
        )
        .await
        .unwrap()
        .status,
        v2_1::ReserveNowStatus::Accepted,
    );
    assert_eq!(
        csms.call(&id, v2_1::CancelReservationRequest::new(1))
            .await
            .unwrap()
            .status,
        v2_1::CancelReservationStatus::Accepted,
    );

    assert_eq!(
        station.handled.load(Ordering::SeqCst),
        17,
        "every CSMS-originated call above has to have reached the station",
    );

    // --- And the station-originated half of the same profiles ----------------
    // These go out through the *station's* handle: the engine refuses an action the peer may
    // not originate, so sending them from the CSMS side is a protocol error, not a shortcut.
    let s = &station_handle;
    let before = csms_handler.handled.load(Ordering::SeqCst);

    assert_eq!(
        s.call(v2_1::SignCertificateRequest::new(
            "-----BEGIN CERTIFICATE REQUEST-----"
        ))
        .await
        .unwrap()
        .status,
        v2_1::GenericStatus::Accepted,
    );
    s.call(v2_1::ReportChargingProfilesRequest::new(
        1,
        "CSO",
        vec![sample_profile()],
        1,
    ))
    .await
    .unwrap();
    s.call(v2_1::NotifyChargingLimitRequest::new(
        v2_1::ChargingLimit::new("CSO"),
    ))
    .await
    .unwrap();
    s.call(v2_1::ClearedChargingLimitRequest::new("CSO"))
        .await
        .unwrap();
    s.call(v2_1::NotifyMonitoringReportRequest::new(
        9,
        0,
        DateTime::now(),
    ))
    .await
    .unwrap();
    s.call(v2_1::NotifyEventRequest::new(
        DateTime::now(),
        0,
        vec![v2_1::EventData::new(
            1,
            DateTime::now(),
            v2_1::EventTrigger::Alerting,
            "81",
            v2_1::Component::new("OCPPCommCtrlr"),
            v2_1::EventNotification::HardWiredMonitor,
            v2_1::Variable::new("HeartbeatInterval"),
        )],
    ))
    .await
    .unwrap();
    s.call(v2_1::NotifyDisplayMessagesRequest::new(11))
        .await
        .unwrap();
    s.call(v2_1::ReservationStatusUpdateRequest::new(
        1,
        v2_1::ReservationUpdateStatus::Expired,
    ))
    .await
    .unwrap();

    // --- ISO 15118: the projections a CSMS actually sees ---------------------
    assert_eq!(
        s.call(v2_1::Get15118EVCertificateRequest::new(
            "urn:iso:15118:2:2013:MsgDef",
            v2_1::CertificateAction::Install,
            "base64-exi",
        ))
        .await
        .unwrap()
        .status,
        v2_1::Iso15118EVCertificateStatus::Accepted,
    );
    assert_eq!(
        s.call(v2_1::GetCertificateStatusRequest::new(
            v2_1::OCSPRequestData::new(
                v2_1::HashAlgorithm::SHA256,
                "aa",
                "bb",
                "01",
                "http://ocsp.example.com",
            ),
        ))
        .await
        .unwrap()
        .status,
        v2_1::GetCertificateStatusEnum::Accepted,
    );
    assert_eq!(
        s.call(v2_1::NotifyEVChargingNeedsRequest::new(
            1,
            v2_1::ChargingNeeds::new(v2_1::EnergyTransferMode::ACThreePhase),
        ))
        .await
        .unwrap()
        .status,
        v2_1::NotifyEVChargingNeedsStatus::Accepted,
    );
    assert_eq!(
        s.call(v2_1::NotifyEVChargingScheduleRequest::new(
            DateTime::now(),
            sample_schedule(),
            1,
        ))
        .await
        .unwrap()
        .status,
        v2_1::GenericStatus::Accepted,
    );

    assert_eq!(
        csms_handler.handled.load(Ordering::SeqCst) - before,
        12,
        "every station-originated call above has to have reached the CSMS",
    );
}

fn sample_schedule() -> v2_1::ChargingSchedule {
    v2_1::ChargingSchedule::new(
        1,
        v2_1::ChargingRateUnit::A,
        vec![v2_1::ChargingSchedulePeriod::new(0)],
    )
}

fn sample_profile() -> v2_1::ChargingProfile {
    v2_1::ChargingProfile::new(
        1,
        0,
        v2_1::ChargingProfilePurpose::TxDefaultProfile,
        v2_1::ChargingProfileKind::Absolute,
        vec![sample_schedule()],
    )
}
