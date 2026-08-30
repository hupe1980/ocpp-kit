+++
title = "ocpp-kit"
sort_by = "weight"
template = "index.html"
+++

```rust
use ocpp_kit::transport::{BasicAuthPassword, ClientTls, Handler, SecurityProfile, Station};
use ocpp_kit::{Version, v2_1};

async fn boot(handler: impl Handler) -> Result<(), Box<dyn std::error::Error>> {
    let handle = Station::builder()
        .identity("CS-0001")?
        .url("wss://csms.example.com/ocpp")
        // Part 4 §3.2 recommends that a 2.1 station also offer 2.0.1.
        .versions([Version::V2_1, Version::V2_0_1])
        .security_profile(SecurityProfile::TlsBasicAuth)
        .password(BasicAuthPassword::utf8("a-sixteen-plus-character-secret")?)
        .tls(ClientTls::with_webpki_roots()?)
        .handler(handler)
        .build()?
        .spawn()?;

    let boot = handle
        .call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        ))
        .await?;

    println!("{:?}, heartbeat every {}s", boot.status, boot.interval);
    Ok(())
}
```
