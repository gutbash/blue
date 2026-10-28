# Building ib-imessage on Windows

Group G runs this helper as its iMessage transport (`GroupG-iMessage` scheduled task in the
groupg repo, `tools/imessage/`). Two tools must be set up before `cargo build --release`.

1. **Perl:** Strawberry Perl must come first on `PATH` (`C:\Strawberry\perl\bin`). Git Bash's
   perl cannot build the vendored OpenSSL.
2. **protoc:** set `PROTOC` to a protoc 28.x binary. On the firm PC it lives at
   `%LOCALAPPDATA%\Programs\protoc\bin\protoc.exe`, and the user env var `PROTOC` points there.

```powershell
$env:PATH = "C:\Strawberry\perl\bin;$env:PATH"
cargo build --release
```

While the service runs, `target\release\ib-imessage.exe` (or whichever exe the service uses) is
locked. Build into another directory with `--target-dir target-next2`, stop the service, copy
the exe over, and start it again. `target-next*` is gitignored.

The registration state (Apple ID keys) lives outside the repo, in
`%USERPROFILE%\.groupg-secrets\ib-imessage-state`. Never commit it.
