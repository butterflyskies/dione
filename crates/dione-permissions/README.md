# Dione permissions

Owns administrator DM prompts and the actual permission component lifecycle.
Current configuration supplies admin authorization at interaction handling time.
Acknowledgement precedes sibling removal, deletion attempts and response events.
Failed acknowledgement retains pending state; duplicate clicks can acknowledge
but cannot emit a second decision. Individual DM/delete errors log and continue.

Shared Discord state owns transient pending records and sent-id suppression.
This crate adds no persistence, expiry, retry policy or request correlation beyond
the existing request-id matching. A parsed component prefix alone is not authority.
No generic responder, earlier admin verdict or private authority constructor is used.
