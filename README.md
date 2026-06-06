# athroisma
*ἄθροισμα - Greek for "aggregate"*

> Part of this codebase was written by [Claude](https://claude.ai) (Anthropic) via Claude Code.

A lightweight Linux system stats process that emits one JSON line per interval to stdout.

## Motivation

Most system monitors are built for dashboards, they assume you want a GUI, a database, or at minimum a process that's acceptable to leave a mark on your CPU graph. Athroisma was built for the opposite use case: desktop ricing, where a bar or widget needs live CPU and GPU numbers but the monitor itself must be invisible in every sense. It runs as a background process, reads only what you ask for, and is fast enough that it doesn't show up in its own output.

That said, nothing about it is rice-specific. Any tool that needs a reliable, low-overhead stream of system stats over stdin/stdout can use it.

## Usage

```
athroisma [interval_ms]
```

Defaults to 1000 ms. Prints JSON forever; pipe it wherever you need it.

## Controlling what gets sampled

Write a line to stdin to tell athroisma what to include. The line is a space-separated list of section names - whatever you list is on, everything else is off. The new request takes effect on the next tick and stays until you send another line.

```
cpu mem gpu net disk procs
```

| Token   | What it enables |
|---------|-----------------|
| `cpu`   | CPU usage percent and load average |
| `mem`   | Memory and swap used/total |
| `gpu`   | Per-card busy percent, temperature, power draw, VRAM |
| `net`   | Network I/O rates per interface |
| `disk`  | Disk I/O rates per device |
| `procs` | Per-process lists inside cpu, mem, and gpu sections |

Omitting `procs` skips the `/proc` walk entirely, which keeps CPU usage near zero. Sections that aren't requested are omitted from the JSON output rather than set to null.

**Examples:**

```
# Just aggregate CPU and GPU, no process lists
cpu gpu

# Full output
cpu mem gpu net disk procs

# Silence everything (athroisma keeps running, outputs {} each tick)

```

On startup, all sections are enabled by default so the process is useful without any stdin input.

## Controlling the interval

Send `interval <ms>` to change the polling rate at runtime:

```
interval 500
interval 2000
```

The argv interval sets the initial value; stdin can override it at any point.

## Output shape

```json
{
  "cpu": {
    "percent": 12.4,
    "load": 1.02,
    "procs": [{ "name": "firefox", "pid": 1234, "cpu": 8.1 }]
  },
  "memory": {
    "used_bytes": 8589934592,
    "total_bytes": 17179869184,
    "swap_used_bytes": 0,
    "swap_total_bytes": 0,
    "procs": [{ "name": "firefox", "pid": 1234, "rss": 524288000 }]
  },
  "gpu": [{
    "card": "card0",
    "busy": 43,
    "temp_c": 65.0,
    "power_w": 120.5,
    "vram_used": 2147483648,
    "vram_total": 8589934592,
    "procs": [{ "name": "blender", "pid": 5678, "vram_kib": 2097152, "gfx_pct": 38.2 }]
  }],
  "net": [
    { "name": "enp15s0", "rx_bytes_per_sec": 204800, "tx_bytes_per_sec": 51200 }
  ],
  "disk": [{
    "name": "sda",
    "read_bytes_per_sec": 0,
    "write_bytes_per_sec": 85924,
    "partitions": [
      { "name": "sda1", "read_bytes_per_sec": 0, "write_bytes_per_sec": 0, "partitions": [] },
      { "name": "sda2", "read_bytes_per_sec": 0, "write_bytes_per_sec": 85924, "partitions": [] }
    ]
  }]
}
```

Disk devices are structured as a tree, partitions are nested under their parent device so you can use whichever level you need.

## What it collects

**CPU** - tick deltas from `/proc/stat`, load average from `/proc/loadavg`

**Memory** - used/total and swap from `/proc/meminfo`

**GPU** - AMD only, via `/sys/class/drm` and hwmon. Busy percent, temperature, power draw, VRAM used/total. Per-process VRAM and graphics-engine usage via `drm-engine-gfx` fdinfo deltas.

**Network** - rx/tx bytes per second per interface from `/proc/net/dev` (loopback excluded)

**Disk** - read/write bytes per second per device from `/proc/diskstats` (loop and ram devices excluded)

**Processes** - top 10 by CPU%, top 10 by RSS, top 10 GPU procs by VRAM. Only sampled when `procs` is requested.

## Building

```
cargo build --release
```
