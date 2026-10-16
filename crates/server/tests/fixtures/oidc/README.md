Test-only RSA keys for tests/google_sign_in.rs: the mock Google signs ID
tokens with google-k1/google-k2 (published in its JWKS) and attacker (never
published). Not secrets; never used outside the test.

Regenerate: `openssl genrsa -traditional 2048 | openssl pkcs8 -topk8 -nocrypt -outform DER > NAME.pk8`
(ring needs PKCS#8 v1 with the NULL algorithm parameter).
