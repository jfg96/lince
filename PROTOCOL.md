# lince protocol, version 1

Clients talk to `lince serve` over a Unix stream socket (default `$XDG_RUNTIME_DIR/lince/lince.sock`, or `$TMPDIR/lince-$UID/lince.sock` when `XDG_RUNTIME_DIR` is unset). Each request is a single line of JSON, and each one gets a single line of JSON back, in order. A client can keep its connection open for as long as it likes.

Every response carries `"v": 1`. Within a version, fields are only ever added. A client should ignore fields it does not know, and treat a different `v` as incompatible.

## Search (`"op": "search"`, the default)

Request (every field is optional):

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | integer | 0 | Echoed back, so clients can drop stale answers. |
| `query` | string | `""` | Whitespace-separated terms, all of which must match (see *Matching*). Empty means browse. |
| `kinds` | `["d","f"]` | both | Directories and/or regular files. Symlinks and special files are never indexed. |
| `max_results` | integer | 2000 | Per kind. When more entries match, the best-ranked ones are kept. |
| `select` | `"best"` \| `"first"` | `"best"` | `"first"` keeps index order instead of ranking. |
| `hidden` | bool | false | Include entries whose name, or an ancestor's name below the root, starts with `.`. |
| `extensions` | strings | none | Files only: the name must end in `.ext` (case-insensitive, with a non-empty stem, as in `fd -e`). |
| `roots` | absolute paths | all | Only these indexed roots. An unknown root is an error. |
| `scopes` | relative paths | none | Only entries below these directories of each root, as if they were fd's search paths. |
| `max_depth` | integer | none | Depth limit below each scope, or below the root when there are no scopes. |
| `exclude` | glob patterns | none | gitignore-style globs, as in `fd --exclude`. Patterns the daemon was started with are already excluded; any others are applied to this request only. |
| `mtimes` | integer | 0 | Return the mtimes of the first N results (directories first). |

Response:

```json
{"v":1,"id":7,"dirs":["/home/me/.config/hypr"],"files":["/home/me/notes/hypr.md"],
 "mtimes":{"/home/me/.config/hypr":1760000000000},
 "matched_dirs":1,"matched_files":1,"engine_us":2140,"phases_us":[900,700,0,540],
 "generation":313871,"fresh":true}
```

- `dirs` and `files` are absolute paths, at most `max_results` of each, in no particular order. Sort them on the client.
- `matched_*` counts every match, including any that were cut by `max_results`.
- `mtimes` maps a path to milliseconds since the epoch.
- `fresh` is `false` when changes may be missing (the daemon runs without a watcher, or inotify ran out of watches). A client that needs certainty should then walk the disk itself.
- Errors look like `{"v":1,"id":7,"error":"root not indexed: /mnt/x"}`.
- If a client sends several requests before reading, only the newest is evaluated. The others get `{"v":1,"id":N,"superseded":true}`.

## Matching

Names are folded once, at index time: lowercased, with combining accents stripped after NFD normalisation. Terms are folded the same way, so `cancion` finds `Canción`, and `españa` finds `espana`. A candidate keeps the first pass it matched:

1. **Name:** every term is a substring of the name. This pass is skipped when any term contains `/`.
2. **Path:** every term is a substring of the path below the entry's root. The root's own path never matches, so a user name in `/home/<user>` does not match every file.
3. **Fuzzy:** used only when the first two passes found fewer than `min(60, max_results)` entries and every term has at least 3 characters and no `/`. Every term must be an ordered subsequence of the name (`omfind` finds `omarchy-find`).

## Ranking (`select: "best"`)

When more than `max_results` entries match, they are ranked by score (lower is better), then non-hidden before hidden, then shorter names, then path. Name lengths count UTF-16 code units, and paths compare as UTF-16 code unit sequences: the length and order a JavaScript or Qt string gives, so such a client can reproduce the ranking with plain string operations. The score is the sum over the query's terms, with a repeated term counted each time. Each term scores against the folded name: 0 if it equals the name, 1 if it is a prefix, 2 if it starts a word (after a space, `-`, `_`, `.` or `/`), and 3 for any other substring. If the term is not in the name, it scores against the folded full path: 4 if it starts a component or a word, 5 for a plain substring, 6 if it is a subsequence of the name, 7 if it is a subsequence of the path. A browse request (empty query) ranks by hidden, then depth, then folded name, then path. A client that sorts its results by these rules shows first the entries lince kept.

## Status (`"op": "status"`)

```json
{"v":1,"version":"0.1.0","roots":["/home/me"],"exclude":[".git","node_modules"],
 "entries":313860,"dead":5,"index_bytes":32594589,"rss_kib":41220,
 "watches":34413,"fresh":true,"generation":313871}
```
