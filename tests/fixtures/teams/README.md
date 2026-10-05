# Teams feasibility fixtures

`connector-private.pem` is a non-secret RSA test key copied from the
MIT-licensed `jsonwebtoken` 11.0.0 test suite. It exists only to create a
deterministic RS256 token inside the hermetic test. It must never be used
outside tests.

`connector-openid.json` and `connector-jwks.json` are frozen representative
Bot Connector wire documents. The public RSA values in the JWKS correspond to
the test-only private key. The verifier must deserialize these bytes rather
than deriving its verification key from the private key.
