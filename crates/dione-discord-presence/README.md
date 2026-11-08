# dione-discord-presence

Shared requested Discord presence, direct and queued acknowledgements, and reconnect replay. Implement the existing PresenceSink for a gateway connection, then share the handles from wire_shared_presence between the gateway and tool transport.

Direct Applied acknowledges stored desired state. Queued Accepted acknowledges queue admission. Neither proves remote Discord delivery. Installing a replacement sink replays the stored request without changing its decision timestamp. Sink installation history does not track disconnects.

Sink callbacks run outside the state lock under ordered effects. They must not wait for another mutation. The wire module owns MCP request decoding and typed result projection. The application retains authenticated dispatch, concrete capability selection and Serenity conversion.

The request decoder preserves absent, null and non-string defaults and status-before-activity errors. A decoded request does not select a capability or authorize execution. Applied and Accepted acknowledge different local progress; neither proves remote delivery.
