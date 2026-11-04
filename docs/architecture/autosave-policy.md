# Autosave scheduling policy

The shared crate provides a deterministic, thread-free scheduler. Applications poll
it from their normal event loop; it does not create a permanent process, timer
thread or background service.

Default policy:

- Debounce: save after 2 seconds without an edit.
- Maximum dirty age: save after 15 seconds even if edits continue.

A save failure leaves the scheduler dirty and due. Only the application may call
mark_saved, and only after the document save and the required recovery state have
completed successfully. The scheduler does not serialize documents or perform
disk I/O itself.

The timing policy is configurable. UI responsiveness, large-document serialization
and recovery-journal frequency still need measurement in each application.
