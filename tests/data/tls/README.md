# TLS test fixtures

P-256, valid for 100 years, generated once with `openssl`. Test-only keys: nothing here is
secret, and the CA private keys were deleted after signing, so nothing new can be issued
under these roots. Excluded from the published package.

| File | What |
|---|---|
| `ca.pem` | Test CA. Trusted by the station for the CSMS, and by the CSMS for stations. |
| `csms.pem` / `csms.key` | CSMS server leaf, `subjectAltName = IP:127.0.0.1`, `serverAuth`. |
| `station.pem` / `station.key` | Station client leaf, `CN=CS-CERT`, `clientAuth`, under `ca.pem`. |
| `rogue-ca.pem` | A second CA nobody trusts. |
| `rogue-station.pem` / `rogue-station.key` | `CN=CS-CERT` again, but under `rogue-ca.pem`. |
