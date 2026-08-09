# DeskLink reliability validation

The current regression coverage is build and unit-test based. Physical-device
validation remains required for portal permissions, Android MediaProjection,
notification-listener resync, and Wi-Fi recovery.

## Commands

```bash
cargo fmt --check
cargo test -- --test-threads=1
cargo clippy -- -D warnings
./gradlew testDebugUnitTest
./gradlew lintDebug
./gradlew assembleDebug
```

## Required observations

- A transfer is published only after size/checksum verification and atomic finalization.
- Cancelled transfers retain a verified checkpoint and temporary file for resume; failed
  transfers clean up their temporary file.
- A payload connection with an invalid token, certificate, size, or timeout is rejected.
- A portal request is not recreated for every input packet.
- A notification resync continues after an invalid third-party icon.
- Screen frames use the shared DeskLink frame envelope in both directions.
