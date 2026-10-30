# Windows release signing

This is the reference policy for adding Authenticode signing through Azure Artifact Signing.
The retained upstream `release.yml` does not currently implement the signing steps or secrets
below. Check the exact release's published artifacts and provenance; do not assume an EXE or MSI
is signed because this procedure is documented. Enabling signing requires a separate reviewed
release-workflow change and administrator configuration.

When enabled, Windows release executables and the MSI should be Authenticode-signed.
The private key is service-managed and must never be exported to GitHub, a runner, a developer
machine, or this repository. GitHub Actions authenticates with a short-lived OpenID Connect token;
the federated service principal receives only the **Artifact Signing Certificate Profile Signer**
role for the release certificate profile.

## Repository configuration

To enable this policy, release administrators configure these GitHub Actions secrets:

- `WINDOWS_SIGNING_AZURE_CLIENT_ID`
- `WINDOWS_SIGNING_AZURE_TENANT_ID`
- `WINDOWS_SIGNING_AZURE_SUBSCRIPTION_ID`
- `WINDOWS_SIGNING_ENDPOINT`
- `WINDOWS_SIGNING_ACCOUNT_NAME`
- `WINDOWS_SIGNING_CERTIFICATE_PROFILE`

They also configure the non-secret repository variable `WINDOWS_SIGNING_SUBJECT` to the exact
certificate subject expected on published files. The Azure federated credential must be restricted
to this repository's protected release-tag environment or equivalent tag subject; it must not trust
arbitrary repositories, branches, or pull requests.

The proposed release workflow signs the full and lite `mini-agent.exe` before packaging and signs the final
MSI after WiX assembly. Every signing call requests SHA-256 file and RFC3161 timestamp digests. The
workflow immediately uses Windows `Get-AuthenticodeSignature` to require `Valid` status, the exact
configured signer subject, and a timestamp certificate. Missing configuration, unavailable signing,
an unexpected identity, or a missing timestamp fails the release before publication.

Any unsigned release exception must be authorized for an exact tag and identified in its
published release notes. No signing-waiver list is configured in the retained upstream workflow;
waivers from the source fork do not authorize releases here.

VSIX packages are covered by release SHA-256 manifests and GitHub build provenance; they are not
Authenticode containers. The Windows executable inside each Windows VSIX must be the same signed
release executable once this policy is enabled.

## Custody and rotation

Azure account owners, certificate-profile operators, and GitHub release administrators should be
separate where organizational policy permits. Audit Azure role assignments and GitHub environment
access at least quarterly. Do not store certificate files, passwords, client secrets, or recovery
material in repository variables, workflow files, artifacts, logs, or Beads records.

To rotate the signing certificate:

1. Create and validate a new Artifact Signing certificate profile.
2. Grant the existing narrowly scoped release principal signer access to that profile.
3. Change `WINDOWS_SIGNING_CERTIFICATE_PROFILE` and `WINDOWS_SIGNING_SUBJECT` together.
4. Run a draft prerelease and retain the workflow's signature identity/timestamp output.
5. Download the draft EXE archive and MSI on a clean Windows host and independently confirm
   `Get-AuthenticodeSignature` reports `Valid` with the new subject.
6. Publish only after the old profile is removed from the release principal or formally retained for
   a documented overlap period.

If the OIDC principal or signing profile is suspected compromised, disable its federated credential
or signer role first, keep releases fail-closed, rotate the identity/profile, and do not bypass the
signature gate to restore publication.
