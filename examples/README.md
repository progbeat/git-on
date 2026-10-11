# Application examples

These are optional configurations, not additional filesystem rules. git-on uses
only the selected tree's `.gitignore` policy for new writes, regardless of the
language or tool. Paths present in that tree remain immutable even if ignored.

## Python cache and environment paths

For a Python application that needs these paths, its committed `.gitignore` can
include:

```gitignore
.venv/
__pycache__/
.pytest_cache/
```

These patterns permit new matching paths; omitting a required pattern means the
corresponding write is denied. Numba's `cache=True` can use `__pycache__` under
this ordinary policy without a special environment override. Add only patterns
appropriate for your application.

## Locked Python examples

Both examples import a local module that uses NumPy and `@numba.njit(cache=True)`.
They run that module in two separate Python processes, assert the second process
reuses the compiled cache without rewriting its files, and print `cache-reused`.
The committed `.gitignore` permits `__pycache__/` and `.venv/` writes.

After committing the examples in a repository, run either snapshot subtree:

```console
git on HEAD:examples/pep723 uv run --locked --script main.py
git on HEAD:examples/uv-project uv run --locked main.py
```

The first example uses PEP 723 inline metadata and `main.py.lock`. The second
uses `pyproject.toml` and `uv.lock`. Both pin NumPy 2.2.6 and Numba 0.61.2; the
lockfiles also pin and hash the llvmlite dependency. Python 3.11–3.13 is required.
CI validates these with uv 0.8.22. Initial execution needs package-index access.

Inside git-on, the writable snapshot layer is temporary. Cache reuse is within
one run; a later run starts fresh. To inspect the generated caches afterward,
retain that layer explicitly:

```console
git on HEAD:examples/pep723 -k -- uv run --locked --script main.py
git on HEAD:examples/uv-project --keep -- uv run --locked main.py
```

After unmounting, git-on prints the retained temporary-directory path on stderr.
The generated local-import and Numba cache files are under its
`backing/__pycache__/` directory. No automatic artifact copy or output-directory environment variable
is involved. Delete the retained directory when you no longer need it.
