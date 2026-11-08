# PluralKit fact resolver

Resolves bounded identity facts from the fixed official HTTPS provider. Message/channel/guild coordinates are checked on fresh responses and cache hits. The facts-only cache retains physical expiry and bounded duplicate-miss behavior; a single timeout covers semaphore admission, cache access, retries, response reads and decoding. Redirects are disabled and responses are bounded to 64 KiB.

`MessageBinding` contains ordinary expected coordinates. Neither it nor returned facts proves Discord transport verification or grants authorization. The embedding retains trusted-creator checks, sealed action transitions, lifecycle admission and all policy. The local HTTP test-origin seam remains private to this crate's tests.
