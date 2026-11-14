# Writing documentation

This site is built with [Zensical](https://zensical.org/) from the Markdown
files in `docs/`. The configuration is `zensical.toml` in the repository
root.

## Requirements

Building the site needs only [uv](https://docs.astral.sh/uv/). `uvx`
downloads Zensical into a temporary environment, so nothing has to be
installed in the project. The full documentation check,
`scripts/check.sh docs`, also needs npx, python3 and git; see
[Running all checks](building.md#running-all-checks).

## Preview

```shell-session
$ uvx zensical serve
```

Open <http://localhost:8000>. The page reloads when a file changes.

## Build

```shell-session
$ uvx zensical build --clean
```

The static site is written to `site/`, which is ignored by Git.

## Adding a page

1.  Create the Markdown file under `docs/`.
2.  Add it to `nav` in `zensical.toml`.
3.  Lint it:

    ```shell-session
    $ npx --yes markdownlint-cli2@0.24.0 "docs/**/*.md"
    ```

## Theme

The palette (green `primary` and `accent` for light and dark schemes) and the
header logo (`lucide/dog`) are set in `zensical.toml`. Additional styling is
in `docs/stylesheets/extra.css`, loaded through `extra_css`:

*   darker green link and active-navigation text in the light scheme, so it
    stays readable on white;
*   a green left bar on `h1` and a green underline on `h2`;
*   bold, green sidebar section titles.

## Style

*   Write in English.
*   Describe what the code does now, not what is planned. Check option names,
    defaults and messages against the source before documenting them.
*   Use `*` for bullets, followed by three spaces, and four-space
    indentation for nested content, as required by `.markdownlint.json`.
*   Use the terms defined in [Concepts](../concepts.md), such as **CUDA
    number** for client device numbers and **daemon GPU number** for the
    daemon's numbering.
*   Give every code block a language. Use `shell-session` for commands with a
    `$` prompt and `text` for program output.

## Review

Review a documentation change against the style rules above and the docs
map in `AGENTS.md` ("Agent profile"): readability, concise but complete
wording, correctness against the code, and consistent terms. Then run
`scripts/check.sh docs`.
