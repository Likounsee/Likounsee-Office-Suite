# Contributing

- Keep engines independent from GUI frameworks.
- Cover behavior with automated tests.
- Run formatting, Clippy and workspace tests before submitting.
- Never silently discard unsupported content during import/export.
- Do not add telemetry, background daemons or network requirements without an
  explicit product decision.
- Document platform-specific behavior and filesystem guarantees.
- Never commit build output, secrets or user documents.
