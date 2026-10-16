# Security policy

## Reporting a vulnerability

Please do not open a public issue. Report it privately through GitHub:
**Security > Report a vulnerability** on
[Rebulk/railwatch-rs](https://github.com/Rebulk/railwatch-rs/security/advisories/new)
(GitHub private vulnerability reporting). Include the version
(`railwatch --version`), what an attacker needs (network access, an
ingest token, an account login, an admin role), and steps or a proof of
concept.

We aim to acknowledge a report within 3 working days and to agree on a fix
and disclosure date with you. Fixes ship as a new release; the advisory is
published once it is out, crediting you unless you prefer otherwise.

## Supported versions

The latest release. `sudo railwatch upgrade` moves an install to it.

## Verifying what you run

Every release's `SHA256SUMS` is signed keyless with cosign by this
repository's release workflow; there is no signing key to steal.
`sudo railwatch upgrade` refuses an unverified release. To check one by
hand (replace `vX.Y.Z` with the release you downloaded):

```sh
sha256sum -c --ignore-missing SHA256SUMS
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity https://github.com/Rebulk/railwatch-rs/.github/workflows/release.yml@refs/tags/vX.Y.Z \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-trigger push \
  --certificate-github-workflow-repository Rebulk/railwatch-rs
```

Releases are reproducible from the tagged commit with `bin/release`
(docs/SELF-HOST.md, "Releases").
