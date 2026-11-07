# Dione Discord scheduling

Classifies current Discord notification evidence and conversation scope, then
uses the existing generic delivery scheduler for buffering, deadlines, bounded
immediate bursts, FIFO flushing, payload rejection, and failed-tail requeue.
The configured module slot keeps identical Discord channel IDs distinct.

The adapter returns original notification payloads; MCP retains conversion,
sending, guards, rate limits, and retry orchestration. Conversationless controls
remain immediate and are discarded when requeued, matching existing behavior.
Scheduling observations do not grant delivery authority.
