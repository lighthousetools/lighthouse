# Release image

Every `v*` tag builds and publishes `ghcr.io/lighthousetools/lighthouse-server`, signed with
[cosign](https://github.com/sigstore/cosign) keyless signing. The workflow is
[`.github/workflows/release-image.yml`](../.github/workflows/release-image.yml). It can also be started by hand, then
it publishes only a `sha-` tag.

## Tags

| Tag | Meaning |
| --- | --- |
| `vX.Y.Z` | The release of the same name |
| `sha-<short>` | The commit of this repository the image was built from |

## What is inside

The image is the `release` target of the [`Dockerfile`](../Dockerfile), for `linux/amd64`:

- `lighthouse-server`, `lht` and `lighthouse-demo-mcp` in `/usr/local/bin`
- the skills, the example agents and automations and the demo files in `/usr/share/lighthouse`
- the built web app from `lighthousetools/lighthouse-web` in `/usr/share/lighthouse/web`

The web app is built from the lighthouse-web tag with the same name as the release if it exists, otherwise from `main`.
The job summary of each run names the web commit and the image digest.

The image sets `LIGHTHOUSE_PATHS__WEB=/usr/share/lighthouse/web`, so the server offers the web app without a separate
checkout. Environment variables override the configuration file; to serve a different build, set
`LIGHTHOUSE_PATHS__WEB` to its directory. Like the plain image, it runs as user `lighthouse` (uid 10001), reads
`/etc/lighthouse/lighthouse.toml` and listens on port 8080.

The default target of the `Dockerfile` stays without the web app; the [demo](../demo/compose.yml) mounts it instead.

## Build it yourself

```sh
(cd ../lighthouse-web && pnpm install --frozen-lockfile && pnpm build)
cp -r ../lighthouse-web/dist web-dist
docker build --target release -t lighthouse-server:local .
```

## Verify the signature

```sh
cosign verify ghcr.io/lighthousetools/lighthouse-server:vX.Y.Z \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github\.com/lighthousetools/lighthouse/\.github/workflows/release-image\.yml@refs/tags/v'
```

Pin a deployment to the digest that `cosign verify` and the job summary show, for example
`ghcr.io/lighthousetools/lighthouse-server@sha256:...`.
