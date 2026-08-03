#!/usr/bin/env python3
"""Rank Proton VPN servers your account can actually use, by measured speed.

Why this exists
---------------
Proton ships a per-server ``Score`` and the client connects to whichever
server has the lowest one. That score is computed server-side from load and
a coarse notion of proximity, and on this machine it is simply wrong:

    from Sydney, Proton's "fastest" free server is Amsterdam at 260 ms,
    while an unranked Tokyo server answers in 163 ms.

So instead of trusting the score, this measures. It shortlists the servers
the account is entitled to, opens a TCP connection to each one's entry IP,
and ranks them on what was actually observed: round-trip time first, then
reported load, then great-circle distance as a stabiliser.

Everything here is offline apart from the probes themselves. The server list
comes from the cache Proton already maintains, and the user's own position is
derived from the system timezone via ``zone.tab`` — no geolocation lookup, no
extra dependency, and nothing that a filtered network can block.

Run ``best-server.py --help`` for usage.
"""
from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import math
import os
import re
import sys
from collections.abc import Iterable, Sequence
from dataclasses import asdict, dataclass
from pathlib import Path

# --- constants ---------------------------------------------------------

DEFAULT_SERVERLIST = Path.home() / ".cache/Proton/VPN/serverlist.json"
ZONE_TAB = Path("/usr/share/zoneinfo/zone.tab")
LOCALTIME_LINK = Path("/etc/localtime")
TIMEZONE_FILE = Path("/etc/timezone")

# Proton servers terminate Stealth, OpenVPN-TCP and WireGuard-TLS on 443.
# It is also the one port a filtering network is least likely to drop, which
# makes it both the most relevant and the most reliable thing to time.
PROBE_PORT = 443

DEFAULT_PROBE_ROUNDS = 2
DEFAULT_PROBE_TIMEOUT = 2.0
DEFAULT_SHORTLIST = 40
DEFAULT_RESULTS = 10

# Measurement happens in two passes. The sweep runs wide and fast to find
# out which servers answer at all; the refine pass re-times only the
# finalists, and does so gently.
#
# The split exists because probes compete with each other for the same
# uplink. A wide burst was observed collapsing every server onto the same
# ~200 ms figure - Singapore and Los Angeles indistinguishable - which would
# have handed the win to whichever server happened to be least delayed by
# our own traffic. Re-timing a handful of finalists at low concurrency costs
# about a second and makes the top of the table trustworthy.
SWEEP_CONCURRENCY = 32
REFINE_CONCURRENCY = 4
DEFAULT_REFINE = 8

EARTH_RADIUS_KM = 6371.0

# Server feature bit flags, mirroring proton.vpn.session.servers.types.
FEATURE_SECURE_CORE = 1 << 0
FEATURE_TOR = 1 << 1
# Secure Core doubles your latency by design and Tor servers are slower
# still; neither belongs in a ranking whose whole purpose is speed.
FEATURES_EXCLUDED_BY_DEFAULT = FEATURE_SECURE_CORE | FEATURE_TOR

STATUS_ENABLED = 1

# How the final rating is composed. Measured latency dominates because it is
# the only term that reflects this network rather than a prediction about it.
# Load matters next: a 90%-loaded server can answer a TCP handshake quickly
# and still crawl once you push traffic through it. Distance carries the
# least weight — it is mostly redundant with latency — but it damps the
# jitter of a single probe run and is what "closest" honestly means.
WEIGHT_LATENCY = 0.60
WEIGHT_LOAD = 0.25
WEIGHT_DISTANCE = 0.15

# A server this loaded is treated as fully saturated when scoring, so the
# difference between 95% and 99% does not swamp a real latency advantage.
LOAD_SATURATION_PERCENT = 95.0


# --- data model --------------------------------------------------------

@dataclass
class Candidate:
    """One logical server, plus everything measured or derived about it."""

    name: str
    entry_ip: str
    country: str
    city: str
    tier: int
    load: int
    proton_score: float
    distance_km: float | None = None
    latency_ms: float | None = None
    rating: float | None = None

    @property
    def reachable(self) -> bool:
        return self.latency_ms is not None


# --- server list -------------------------------------------------------

def load_server_list(path: Path) -> tuple[int, list[dict]]:
    """Read Proton's cached server list.

    :returns: the account's max tier and the raw logical server dicts.
    :raises FileNotFoundError: if Proton has never cached a server list.
    """
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
    return int(data.get("MaxTier", 0)), data.get("LogicalServers", [])


def eligible_servers(
    logicals: Iterable[dict],
    max_tier: int,
    country: str | None = None,
    free_only: bool = False,
    excluded_features: int = FEATURES_EXCLUDED_BY_DEFAULT,
) -> list[Candidate]:
    """Select the servers this account is allowed to connect to.

    The tier check is the same one Proton's own client applies
    (``server.tier <= user_tier``); everything else is a speed filter.
    """
    tier_ceiling = 0 if free_only else max_tier
    wanted_country = country.upper() if country else None
    candidates: list[Candidate] = []

    for server in logicals:
        if server.get("Status") != STATUS_ENABLED:
            continue
        tier = int(server.get("Tier", 99))
        if tier > tier_ceiling:
            continue
        if int(server.get("Features", 0)) & excluded_features:
            continue
        if wanted_country and server.get("ExitCountry", "").upper() != wanted_country:
            continue

        entry_ip = _first_enabled_entry_ip(server.get("Servers", []))
        if not entry_ip:
            continue

        candidates.append(Candidate(
            name=server.get("Name", "?"),
            entry_ip=entry_ip,
            country=server.get("ExitCountry", "??"),
            city=server.get("City") or server.get("Region") or "",
            tier=tier,
            load=int(server.get("Load", 100)),
            proton_score=float(server.get("Score") or 0.0),
        ))

    return candidates


def _first_enabled_entry_ip(physicals: Sequence[dict]) -> str | None:
    """Pick an entry IP from a logical server's physical nodes."""
    for physical in physicals:
        if physical.get("Status") != STATUS_ENABLED:
            continue
        if physical.get("ServicesDown"):
            continue
        entry_ip = physical.get("EntryIP")
        if entry_ip:
            return entry_ip
    return None


# --- geography ---------------------------------------------------------

def local_coordinates() -> tuple[float, float] | None:
    """Approximate the user's position from the system timezone.

    ``zone.tab`` maps every IANA timezone to the coordinates of its
    reference city. That is accurate to a few hundred kilometres, which is
    ample for ranking servers that are continents apart, and it costs no
    network request — the point being that this has to work on exactly the
    networks where a geolocation API would be blocked.
    """
    zone = _local_timezone_name()
    if not zone or not ZONE_TAB.exists():
        return None

    for line in ZONE_TAB.read_text(encoding="utf-8").splitlines():
        if line.startswith("#") or not line.strip():
            continue
        fields = line.split("\t")
        if len(fields) >= 3 and fields[2] == zone:
            return parse_iso6709(fields[1])
    return None


def _local_timezone_name() -> str | None:
    """Return the IANA timezone name, e.g. ``Australia/Sydney``."""
    tz_env = os.environ.get("TZ")
    if tz_env and "/" in tz_env:
        return tz_env

    if LOCALTIME_LINK.exists():
        resolved = str(LOCALTIME_LINK.resolve())
        marker = "/zoneinfo/"
        if marker in resolved:
            return resolved.split(marker, 1)[1]

    if TIMEZONE_FILE.exists():
        return TIMEZONE_FILE.read_text(encoding="utf-8").strip() or None

    return None


ISO6709_PATTERN = re.compile(
    r"^([+-])(\d{2})(\d{2})(\d{2})?([+-])(\d{3})(\d{2})(\d{2})?$"
)


def parse_iso6709(text: str) -> tuple[float, float] | None:
    """Parse zone.tab's ``±DDMM±DDDMM`` / ``±DDMMSS±DDDMMSS`` coordinates."""
    match = ISO6709_PATTERN.match(text.strip())
    if not match:
        return None

    lat_sign, lat_d, lat_m, lat_s, lon_sign, lon_d, lon_m, lon_s = match.groups()
    latitude = _to_degrees(lat_sign, lat_d, lat_m, lat_s)
    longitude = _to_degrees(lon_sign, lon_d, lon_m, lon_s)
    return latitude, longitude


def _to_degrees(sign: str, degrees: str, minutes: str, seconds: str | None) -> float:
    value = int(degrees) + int(minutes) / 60 + (int(seconds) / 3600 if seconds else 0)
    return -value if sign == "-" else value


def haversine_km(origin: tuple[float, float], target: tuple[float, float]) -> float:
    """Great-circle distance in kilometres between two lat/long pairs."""
    lat1, lon1 = math.radians(origin[0]), math.radians(origin[1])
    lat2, lon2 = math.radians(target[0]), math.radians(target[1])
    dlat, dlon = lat2 - lat1, lon2 - lon1
    a = math.sin(dlat / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin(dlon / 2) ** 2
    return 2 * EARTH_RADIUS_KM * math.asin(math.sqrt(a))


def annotate_distances(
    candidates: Iterable[Candidate],
    logicals_by_name: dict[str, dict],
    origin: tuple[float, float] | None,
) -> None:
    """Fill in ``distance_km`` for each candidate, when we know where we are."""
    if origin is None:
        return
    for candidate in candidates:
        location = (logicals_by_name.get(candidate.name) or {}).get("Location") or {}
        latitude, longitude = location.get("Lat"), location.get("Long")
        if latitude is None or longitude is None:
            continue
        candidate.distance_km = haversine_km(origin, (float(latitude), float(longitude)))


# --- shortlisting ------------------------------------------------------

def shortlist(candidates: Sequence[Candidate], limit: int) -> list[Candidate]:
    """Cut the pool down to the servers worth spending a probe on.

    Probing all 100+ free servers would work but is wasteful, so this keeps
    the nearest ones (distance predicts latency well) and the least loaded
    ones. Both criteria contribute, because the nearest server is useless if
    it is saturated and a quiet server is useless if it is a continent away.
    """
    if limit <= 0 or len(candidates) <= limit:
        return list(candidates)

    have_distance = any(c.distance_km is not None for c in candidates)
    if have_distance:
        primary = sorted(candidates, key=lambda c: (c.distance_km is None, c.distance_km or 0.0))
    else:
        # No idea where we are: fall back to Proton's own ordering.
        primary = sorted(candidates, key=lambda c: c.proton_score)

    by_load = sorted(candidates, key=lambda c: c.load)

    # Interleave so neither criterion can monopolise the shortlist.
    picked: dict[str, Candidate] = {}
    for index in range(len(candidates)):
        for candidate in (primary[index], by_load[index]):
            if len(picked) >= limit:
                return list(picked.values())
            picked.setdefault(candidate.name, candidate)

    return list(picked.values())


# --- probing -----------------------------------------------------------

async def _probe_once(host: str, port: int, timeout: float) -> float | None:
    """Time a single TCP handshake, in milliseconds. ``None`` if unreachable."""
    loop = asyncio.get_running_loop()
    started = loop.time()
    writer = None
    try:
        _, writer = await asyncio.wait_for(
            asyncio.open_connection(host, port), timeout=timeout
        )
        return (loop.time() - started) * 1000
    except (OSError, asyncio.TimeoutError):
        return None
    finally:
        if writer is not None:
            writer.close()
            with contextlib.suppress(OSError, asyncio.TimeoutError):
                await writer.wait_closed()


async def _probe_candidate(
    candidate: Candidate,
    rounds: int,
    timeout: float,
    semaphore: asyncio.Semaphore,
) -> None:
    """Probe one server several times and keep the best result.

    The minimum, not the mean: a slow sample means something queued
    somewhere, while the fastest handshake is the closest thing we have to
    the true path latency.
    """
    async with semaphore:
        timings = [
            result for result in
            [await _probe_once(candidate.entry_ip, PROBE_PORT, timeout) for _ in range(rounds)]
            if result is not None
        ]
    # Best across all passes, not just this one. measure() probes twice - a
    # wide sweep, then a refine on the leaders - and assigning here let a
    # slower refine sample bury a good sweep result. Worse, a refine that
    # timed out entirely reset this to None, marking a server we had
    # already reached as unreachable.
    best = min(timings, default=None)
    if best is not None and (candidate.latency_ms is None or best < candidate.latency_ms):
        candidate.latency_ms = best


async def probe_all(
    candidates: Sequence[Candidate],
    rounds: int = DEFAULT_PROBE_ROUNDS,
    timeout: float = DEFAULT_PROBE_TIMEOUT,
    concurrency: int = SWEEP_CONCURRENCY,
) -> None:
    """Measure every candidate concurrently, in place."""
    semaphore = asyncio.Semaphore(max(1, concurrency))
    await asyncio.gather(*(
        _probe_candidate(candidate, rounds, timeout, semaphore)
        for candidate in candidates
    ))


async def measure(
    candidates: Sequence[Candidate],
    rounds: int = DEFAULT_PROBE_ROUNDS,
    timeout: float = DEFAULT_PROBE_TIMEOUT,
    refine: int = DEFAULT_REFINE,
) -> list[Candidate]:
    """Sweep every candidate, then re-time the finalists without contention.

    :returns: all candidates, ranked best first.
    """
    await probe_all(candidates, rounds=1, timeout=timeout, concurrency=SWEEP_CONCURRENCY)
    ranked = rank(candidates)

    finalists = [c for c in ranked if c.reachable][:refine]
    if finalists and rounds > 0:
        await probe_all(
            finalists, rounds=rounds, timeout=timeout, concurrency=REFINE_CONCURRENCY
        )
        ranked = rank(candidates)

    return ranked


# --- ranking -----------------------------------------------------------

def _normalise(value: float, low: float, high: float) -> float:
    """Scale ``value`` into 0..1 against an observed range."""
    if high <= low:
        return 0.0
    return (value - low) / (high - low)


def rank(candidates: Sequence[Candidate]) -> list[Candidate]:
    """Score and sort candidates, best first.

    Each term is normalised against the range actually observed in this run,
    so the weights describe relative importance rather than pretending that
    milliseconds and percentages are comparable units. The published
    ``rating`` is 0-100, higher being better, because a number that goes up
    when things get better is the one people read correctly.
    """
    reachable = [c for c in candidates if c.reachable]
    unreachable = [c for c in candidates if not c.reachable]

    if reachable:
        latencies = [c.latency_ms for c in reachable]
        loads = [float(c.load) for c in reachable]
        distances = [c.distance_km for c in reachable if c.distance_km is not None]

        latency_low, latency_high = min(latencies), max(latencies)
        load_low = min(loads)
        load_high = max(load_low, min(max(loads), LOAD_SATURATION_PERCENT))
        distance_low = min(distances) if distances else 0.0
        distance_high = max(distances) if distances else 0.0

        for candidate in reachable:
            cost = (
                WEIGHT_LATENCY * _normalise(candidate.latency_ms, latency_low, latency_high)
                + WEIGHT_LOAD * _normalise(min(float(candidate.load), LOAD_SATURATION_PERCENT),
                                           load_low, load_high)
            )
            if candidate.distance_km is not None and distances:
                cost += WEIGHT_DISTANCE * _normalise(
                    candidate.distance_km, distance_low, distance_high
                )
            candidate.rating = round(100.0 * (1.0 - cost), 1)

    for candidate in unreachable:
        candidate.rating = None

    reachable.sort(key=lambda c: (-(c.rating or 0.0), c.latency_ms))
    unreachable.sort(key=lambda c: (c.distance_km if c.distance_km is not None else math.inf))
    return reachable + unreachable


def rank_without_probing(candidates: Sequence[Candidate]) -> list[Candidate]:
    """Order by prediction alone, for when probing is turned off.

    Distance stands in for latency; Proton's score breaks ties. This is
    strictly worse than measuring and exists only so ``--no-probe`` still
    returns something sensible.
    """
    ordered = sorted(candidates, key=lambda c: (
        c.distance_km if c.distance_km is not None else math.inf,
        c.load,
        c.proton_score,
    ))
    for candidate in ordered:
        candidate.rating = None
    return ordered


# --- presentation ------------------------------------------------------

def render_table(results: Sequence[Candidate], origin: tuple[float, float] | None) -> str:
    """Human-readable ranking, best first."""
    if not results:
        return "No servers matched."

    header = (
        f"{'#':>2}  {'SERVER':<14} {'LOCATION':<22} "
        f"{'PING':>8} {'LOAD':>5} {'DIST':>8}  RATING"
    )
    lines = [header]
    lines.append("-" * len(header))

    for position, candidate in enumerate(results, start=1):
        location = f"{candidate.city}, {candidate.country}" if candidate.city else candidate.country
        ping = f"{candidate.latency_ms:.0f} ms" if candidate.reachable else "--"
        distance = f"{candidate.distance_km:,.0f} km" if candidate.distance_km is not None else "--"
        rating = f"{candidate.rating:.1f}" if candidate.rating is not None else "--"
        lines.append(
            f"{position:>2}  {candidate.name:<14} {location:<22} "
            f"{ping:>8} {candidate.load:>4}% {distance:>8}  {rating}"
        )

    if origin:
        lines.append("")
        lines.append(f"Distances measured from {origin[0]:.2f}, {origin[1]:.2f} (system timezone).")
    return "\n".join(lines)


def render_json(results: Sequence[Candidate], origin: tuple[float, float] | None) -> str:
    return json.dumps({
        "origin": {"lat": origin[0], "long": origin[1]} if origin else None,
        "weights": {
            "latency": WEIGHT_LATENCY,
            "load": WEIGHT_LOAD,
            "distance": WEIGHT_DISTANCE,
        },
        "results": [asdict(candidate) for candidate in results],
    }, indent=2)


# --- entry point -------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="best-server.py",
        description="Rank usable Proton VPN servers by measured latency, load and distance.",
    )
    parser.add_argument("--serverlist", type=Path, default=DEFAULT_SERVERLIST,
                        help="path to Proton's cached serverlist.json")
    parser.add_argument("--country", help="restrict to one exit country code, e.g. JP")
    parser.add_argument("--free", action="store_true",
                        help="only free-tier servers, even on a paid account")
    parser.add_argument("--limit", type=int, default=DEFAULT_RESULTS,
                        help=f"how many results to show (default {DEFAULT_RESULTS}, 0 for all)")
    parser.add_argument("--shortlist", type=int, default=DEFAULT_SHORTLIST,
                        help=f"how many servers to probe (default {DEFAULT_SHORTLIST}, 0 for all)")
    parser.add_argument("--probes", type=int, default=DEFAULT_PROBE_ROUNDS,
                        help=f"re-probes per finalist (default {DEFAULT_PROBE_ROUNDS})")
    parser.add_argument("--refine", type=int, default=DEFAULT_REFINE,
                        help=f"finalists to re-time without contention "
                             f"(default {DEFAULT_REFINE})")
    parser.add_argument("--probe-timeout", type=float, default=DEFAULT_PROBE_TIMEOUT,
                        help=f"seconds before a probe is a miss (default {DEFAULT_PROBE_TIMEOUT})")
    parser.add_argument("--no-probe", action="store_true",
                        help="skip measurement; rank by distance and load only")
    parser.add_argument("--format", choices=("table", "json", "names"), default="table",
                        help="table for people, json for tools, names for scripts")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)

    try:
        max_tier, logicals = load_server_list(args.serverlist)
    except FileNotFoundError:
        print(f"No cached server list at {args.serverlist}.", file=sys.stderr)
        print("Run 'pvpn up' once, or 'protonvpn servers', to populate it.", file=sys.stderr)
        return 2
    except (json.JSONDecodeError, OSError) as error:
        print(f"Could not read {args.serverlist}: {error}", file=sys.stderr)
        return 2

    candidates = eligible_servers(
        logicals, max_tier, country=args.country, free_only=args.free
    )
    if not candidates:
        scope = f" in {args.country.upper()}" if args.country else ""
        print(f"No servers available to this account{scope}.", file=sys.stderr)
        return 1

    origin = local_coordinates()
    annotate_distances(candidates, {s.get("Name"): s for s in logicals}, origin)

    if args.no_probe:
        results = rank_without_probing(candidates)
    else:
        probed = shortlist(candidates, args.shortlist)
        results = asyncio.run(measure(
            probed,
            rounds=max(1, args.probes),
            timeout=args.probe_timeout,
            refine=max(1, args.refine),
        ))
        if not any(candidate.reachable for candidate in results):
            print("No server answered on port 443 - the network may be blocking them.",
                  file=sys.stderr)
            results = rank_without_probing(candidates)

    if args.limit > 0:
        results = results[:args.limit]

    if args.format == "json":
        print(render_json(results, origin))
    elif args.format == "names":
        print("\n".join(candidate.name for candidate in results))
    else:
        print(render_table(results, origin))

    return 0


if __name__ == "__main__":
    sys.exit(main())
