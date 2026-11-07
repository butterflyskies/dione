# Dione delivery scheduler

The scheduler buffers caller-owned payloads by complete provider, module slot,
and conversation identity. It preserves FIFO within each conversation, keeps the
first buffered deadline, and limits eligible immediate deliveries to four per
conversation per one-second window. Control payloads without a conversation
bypass buffering. Callers classify payloads and supply their delay.

Rejected deadlines return the original payload without changing queued events.
Requeue prepends failed events in their original order. Flush operations sort
complete conversation keys and preserve each conversation's FIFO.

The crate uses the Tokio clock. It does not persist events, authorize sources,
classify native evidence, enforce rate limits, or send messages.
