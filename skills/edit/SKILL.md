---
name: edit
description: Replace one exact, unique string in an existing file. Use for a targeted single-occurrence edit from IPython when rewriting the whole file would be less precise.
---

# Edit

Replace one exact, unique occurrence of `old_str` in an existing file.
`old_str` must occur exactly once.

Call the prepared async function from the IPython kernel:

```python
await edit(path="pkg/file.py", old_str=old, new_str=new)
```

Use exact old and new strings. When the text contains triple double quotes, use
triple single-quoted variables such as `old = '''...'''`, or construct `old`
and `new` from file slices already inspected. The call returns a short
confirmation. It raises an error when `old_str` is missing or matches more than
once. Widen the inspected snippet until it is unique.

For a file on a remote host, pass `ssh="host"` (and optional `ssh_options`), the
same transport as `rlm.bash`. `path` is then a remote path and the host needs
`python3`:

```python
await edit(path="srv/config.toml", old_str=old, new_str=new, ssh="core@thumper")
```

When the skill's shell command is available, run it in a shell cell:

```python
%%bash
edit --path pkg/file.py --old-str "..." --new-str "..."
```
