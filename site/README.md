# bgpviews site

`index.html` is the page served at bgpviews.org. One file, inline CSS, no external assets, no
build step.

It is a Cloudflare Pages project named `bgpviews`, deployed from this directory:

    npx wrangler pages deploy site --project-name bgpviews --commit-dirty=true

Preview it locally before deploying:

    python3 -m http.server 8123 --directory site

`<meta name="bgpviews-version">` in the head is the page's own statement of the release it
describes, and `./lab/docs.sh` fails when it disagrees with the version in `Cargo.toml`, so the
page cannot drift a release behind the code again.
