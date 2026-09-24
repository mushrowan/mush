---
name: websearch
description: Search the web through mush's search providers (Exa by default, or a configured SearXNG instance). Takes one query and returns titles, URLs, and page content for the most relevant results.
---

# Web Search

Search the web via Exa, or via a SearXNG instance when one is configured.

## Setup

Exa needs no key. To search through your own SearXNG instance instead, add it
to mush's `config.toml`:

```toml
[web_search]
default_provider = "searx"

[web_search.searx]
base_url = "https://searx.example.org"
```

`[web_search.searx.auth]` takes `type = "basic"` (`username`, `password`),
`type = "bearer"` (`token`), or `type = "header"` (`name`, `value`) when the
instance needs it.

## Usage

Call the prepared `websearch` import directly in the Python kernel:

```python
print(await websearch("latest mush release"))
print(await websearch("rust 2026 edition", num_results=3))
```

`num_results` defaults to 8.
