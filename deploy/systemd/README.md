# systemd units for a host-run mindex

Reference units for the deployment described in `deploy/gate/` — mindex, Qdrant
and the BGE-M3 embedder on one machine, reachable from outside only through the
gateway. They are the units this project's own host runs, with its paths left in
place: read them as a worked example, not as a drop-in.

The point of writing them down is not the boilerplate. It is the four things
below, each of which was **measured** here, and each of which fails in a way that
produces no error.

## Why these are system units, not `--user` units

All three ran under the user manager, and the filesystem sandboxing worked there
— mount namespaces need no privilege. The network confinement does not work
there, and does not say so.

`IPAddressDeny=`, `SocketBindDeny=` and `RestrictNetworkInterfaces=` are enforced
by BPF programs the *manager* loads. A user manager needs bpf cgroup delegation
to load them, and on a typical host it has none (`DelegateControllers=cpu memory
pids`) while `kernel.unprivileged_bpf_disabled=2` forbids it outright. systemd
does not fail the unit over this. The unit starts, the directives are inert, and
`systemd-analyze security` scores it as confined.

Measured 2026-08-04: a throwaway user unit carrying `IPAddressDeny=any
IPAddressAllow=localhost` fetched `https://example.com` and got HTTP 200. The
same directives in a system unit made the same fetch time out while loopback
still answered 200.

The second reason is ordering. A system unit cannot order itself after another
manager's units — `After=user@1000.service` waits for the manager, not for what
it starts. While Qdrant and the embedder were user units, "the store is up before
mindex" was a race that lingering happened to win.

## `IPAddressDeny=` drops packets; libraries hang rather than fail

Everything mindex speaks to is on loopback (embedder, Qdrant, Ollama), so
`IPAddressAllow=localhost` costs nothing — except that both mindex and the
embedder fetch BGE-M3 through a Hugging Face hub client that falls back to
`huggingface.co`. Denied traffic is **dropped, not refused**, so that fallback
does not fail: it waits out a TCP connect timeout and retries.

Measured on the embedder's first boot under confinement: the model load sat at
`Loading model …` for four minutes at 2% CPU with every thread in `futex_wait`,
while `/health` answered `{"status":"ok","model_loaded":false}` throughout. The
previous user unit loaded in 3.3 s purely because the directive was inert there.

Two fixes, one per language:

- **Python** (embedder): `HF_HUB_OFFLINE=1` and `TRANSFORMERS_OFFLINE=1`. The
  cache becomes the whole truth and a miss is an immediate, named error.
- **Rust** (mindex): the `hf-hub` build in use honours `HF_HOME` but has no
  offline switch, so mindex is pointed at a copy it **owns** — five files, 22 MB,
  inside its own data directory beside the database. The default location was the
  shared `~/.cache/huggingface`, which is a *cache*: 48 GB that a disk cleanup is
  entitled to evict, taking the server with it. A tokenizer is a dependency, not
  a cache, and now lives like one.

## `PrivateDevices=true` and `DeviceAllow=` do not compose

`PrivateDevices=true` replaces `/dev` with a minimal private instance.
`DeviceAllow=` grants cgroup permission for a node that is then **not present**
in it. Together they read as "a private `/dev` containing exactly these devices",
and are not that.

Measured on the embedder: `/dev/dri` did not exist inside the namespace, torch
logged `XPU device count is zero!` as a *UserWarning*, and the server started,
answered `/health` "ok", and would have run the whole index on CPU — roughly 17×
slower, with no line anywhere saying why.

So `/dev` stays real and the restriction is done with `DevicePolicy=closed` plus
the `DeviceAllow=` list. The check that it is still armed is not `/health`, which
cannot tell: it is `Model loaded … target_devices=['xpu']` in the journal, and
the absence of the zero-device warning.

## A syscall filter is a time bomb aimed at the code path you did not exercise

`SystemCallFilter=~@resources` killed Qdrant — **not at startup**. Qdrant's HNSW
builder lowers its own thread priority with `sched_setscheduler(2)`, syscall 144,
inside `@resources`; a denied syscall under `SystemCallFilter=` is `SIGSYS`, not
`EPERM`.

Measured: the unit started clean, answered `/health` "ok" for nine minutes,
recovered every collection, and then core-dumped (1.1 GB, thread
`hnsw-build-0`) the first time a write reached the index builder.
`RestrictRealtime=true` is kept, since it refuses `SCHED_FIFO`/`SCHED_RR` —
Qdrant asks for the opposite direction.

The general rule this leaves: **the only honest test of a sandbox is to exercise
the service, not to start it.** Start it, then index a file, then search for it.
Every failure above passed a start-and-check-health test.

## Installing

```sh
sudo install -m0644 deploy/systemd/*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now qdrant mindex-embedder@igpu mindex
```

The embedder is a template; `@igpu` and `@egpu` select the torch backend and are
mutually exclusive (see the unit's own `Conflicts=`). Ordering is declared by
Qdrant and the embedder (`Before=mindex.service`) rather than by mindex, so the
chain reads in one direction and mindex needs no knowledge of their unit names.

Verify, in this order — the last two are the ones that catch what the first
misses:

```sh
systemctl show mindex -p IPAddressDeny -p RestrictNetworkInterfaces
curl -sk https://127.0.0.1:11111/health          # all dependencies "ok"
mindex-index --root . --include '<some file>' --force   # a real GPU encode + Qdrant write
```
