#!/usr/bin/env python3
"""Tests for lib/best-server.py.

These run offline. The only sockets opened are to a listener this file
starts on loopback, so the suite is safe on a filtered network, in CI, or
with the VPN up.

Run them with ``tests/run-tests.sh``, or directly:

    python3 -m unittest discover -s tests -v
"""
from __future__ import annotations

import asyncio
import importlib.util
import io
import json
import socket
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def _load_best_server():
    """Import lib/best-server.py, whose hyphen blocks a normal import."""
    path = REPO_ROOT / "lib" / "best-server.py"
    spec = importlib.util.spec_from_file_location("best_server", path)
    module = importlib.util.module_from_spec(spec)
    # dataclasses resolves annotations through sys.modules, so register the
    # module before executing it.
    sys.modules["best_server"] = module
    spec.loader.exec_module(module)
    return module


bs = _load_best_server()


# --- fixtures ----------------------------------------------------------

def make_logical(
    name="XX-FREE#1",
    tier=0,
    load=50,
    score=1.0,
    country="XX",
    city="Nowhere",
    status=1,
    features=0,
    lat=0.0,
    long=0.0,
    entry_ip="203.0.113.1",
    physical_status=1,
    services_down=0,
):
    """Build one logical server dict shaped like Proton's serverlist.json."""
    return {
        "Name": name,
        "ExitCountry": country,
        "EntryCountry": country,
        "Tier": tier,
        "Load": load,
        "Score": score,
        "City": city,
        "Status": status,
        "Features": features,
        "Location": {"Lat": lat, "Long": long},
        "Servers": [{
            "EntryIP": entry_ip,
            "ExitIP": entry_ip,
            "Status": physical_status,
            "ServicesDown": services_down,
        }],
    }


def make_serverlist(logicals, max_tier=0):
    return {"MaxTier": max_tier, "LogicalServers": logicals}


def candidate(name="A", latency=None, load=50, distance=None):
    return bs.Candidate(
        name=name, entry_ip="203.0.113.1", country="XX", city="Nowhere",
        tier=0, load=load, proton_score=1.0,
        distance_km=distance, latency_ms=latency,
    )


# --- geography ---------------------------------------------------------

class TestIso6709(unittest.TestCase):
    def test_parses_degrees_and_minutes(self):
        latitude, longitude = bs.parse_iso6709("-3352+15113")
        self.assertAlmostEqual(latitude, -(33 + 52 / 60), places=6)
        self.assertAlmostEqual(longitude, 151 + 13 / 60, places=6)

    def test_parses_seconds_when_present(self):
        latitude, longitude = bs.parse_iso6709("+523722+0045409")
        self.assertAlmostEqual(latitude, 52 + 37 / 60 + 22 / 3600, places=6)
        self.assertAlmostEqual(longitude, 4 + 54 / 60 + 9 / 3600, places=6)

    def test_keeps_western_and_southern_signs(self):
        latitude, longitude = bs.parse_iso6709("-2333-04653")
        self.assertLess(latitude, 0)
        self.assertLess(longitude, 0)

    def test_rejects_malformed_input(self):
        for text in ("", "nonsense", "3352+15113", "-33+151"):
            with self.subTest(text=text):
                self.assertIsNone(bs.parse_iso6709(text))


class TestHaversine(unittest.TestCase):
    SYDNEY = (-33.87, 151.22)
    SINGAPORE = (1.29, 103.85)
    AMSTERDAM = (52.37, 4.89)

    def test_known_distance_is_close(self):
        # Sydney to Singapore is about 6,300 km.
        distance = bs.haversine_km(self.SYDNEY, self.SINGAPORE)
        self.assertAlmostEqual(distance, 6300, delta=100)

    def test_zero_for_same_point(self):
        self.assertAlmostEqual(bs.haversine_km(self.SYDNEY, self.SYDNEY), 0.0, places=6)

    def test_symmetric(self):
        there = bs.haversine_km(self.SYDNEY, self.AMSTERDAM)
        back = bs.haversine_km(self.AMSTERDAM, self.SYDNEY)
        self.assertAlmostEqual(there, back, places=6)

    def test_orders_singapore_nearer_than_amsterdam(self):
        # The whole point of the feature: from Sydney, Proton ranks Amsterdam
        # first, but Singapore is less than half the distance away.
        self.assertLess(
            bs.haversine_km(self.SYDNEY, self.SINGAPORE),
            bs.haversine_km(self.SYDNEY, self.AMSTERDAM),
        )


# --- filtering ---------------------------------------------------------

class TestEligibleServers(unittest.TestCase):
    def test_excludes_servers_above_the_account_tier(self):
        logicals = [
            make_logical(name="FREE", tier=0),
            make_logical(name="PLUS", tier=2),
        ]
        names = [c.name for c in bs.eligible_servers(logicals, max_tier=0)]
        self.assertEqual(names, ["FREE"])

    def test_includes_paid_servers_for_a_paid_account(self):
        logicals = [
            make_logical(name="FREE", tier=0),
            make_logical(name="PLUS", tier=2),
        ]
        names = sorted(c.name for c in bs.eligible_servers(logicals, max_tier=2))
        self.assertEqual(names, ["FREE", "PLUS"])

    def test_free_only_overrides_a_paid_account(self):
        logicals = [
            make_logical(name="FREE", tier=0),
            make_logical(name="PLUS", tier=2),
        ]
        names = [c.name for c in bs.eligible_servers(logicals, max_tier=2, free_only=True)]
        self.assertEqual(names, ["FREE"])

    def test_excludes_disabled_servers(self):
        logicals = [make_logical(name="DOWN", status=0)]
        self.assertEqual(bs.eligible_servers(logicals, max_tier=0), [])

    def test_excludes_secure_core_and_tor(self):
        logicals = [
            make_logical(name="SC", features=bs.FEATURE_SECURE_CORE),
            make_logical(name="TOR", features=bs.FEATURE_TOR),
            make_logical(name="PLAIN", features=0),
            make_logical(name="IPV6", features=16),
        ]
        names = sorted(c.name for c in bs.eligible_servers(logicals, max_tier=0))
        self.assertEqual(names, ["IPV6", "PLAIN"])

    def test_filters_by_country_case_insensitively(self):
        logicals = [
            make_logical(name="JP-FREE#1", country="JP"),
            make_logical(name="SG-FREE#1", country="SG"),
        ]
        names = [c.name for c in bs.eligible_servers(logicals, max_tier=0, country="jp")]
        self.assertEqual(names, ["JP-FREE#1"])

    def test_skips_servers_with_no_usable_entry_ip(self):
        logicals = [
            make_logical(name="NODE-DOWN", physical_status=0),
            make_logical(name="SERVICES-DOWN", services_down=1),
        ]
        self.assertEqual(bs.eligible_servers(logicals, max_tier=0), [])

    def test_carries_metadata_onto_the_candidate(self):
        logicals = [make_logical(name="JP-FREE#9", load=81, score=5.0, country="JP", city="Tokyo")]
        result = bs.eligible_servers(logicals, max_tier=0)[0]
        self.assertEqual(result.name, "JP-FREE#9")
        self.assertEqual(result.load, 81)
        self.assertEqual(result.city, "Tokyo")
        self.assertEqual(result.country, "JP")
        self.assertEqual(result.proton_score, 5.0)


class TestAnnotateDistances(unittest.TestCase):
    def test_fills_distance_from_origin(self):
        logicals = [make_logical(name="SG", lat=1.29, long=103.85)]
        candidates = bs.eligible_servers(logicals, max_tier=0)
        bs.annotate_distances(candidates, {"SG": logicals[0]}, origin=(-33.87, 151.22))
        self.assertAlmostEqual(candidates[0].distance_km, 6300, delta=100)

    def test_leaves_distance_unset_without_an_origin(self):
        logicals = [make_logical(name="SG", lat=1.29, long=103.85)]
        candidates = bs.eligible_servers(logicals, max_tier=0)
        bs.annotate_distances(candidates, {"SG": logicals[0]}, origin=None)
        self.assertIsNone(candidates[0].distance_km)


# --- shortlisting ------------------------------------------------------

class TestShortlist(unittest.TestCase):
    def test_returns_everything_when_under_the_limit(self):
        candidates = [candidate(name=str(i)) for i in range(3)]
        self.assertEqual(len(bs.shortlist(candidates, 10)), 3)

    def test_zero_limit_means_no_limit(self):
        candidates = [candidate(name=str(i)) for i in range(30)]
        self.assertEqual(len(bs.shortlist(candidates, 0)), 30)

    def test_respects_the_limit(self):
        candidates = [candidate(name=str(i), distance=float(i)) for i in range(50)]
        self.assertLessEqual(len(bs.shortlist(candidates, 10)), 10)

    def test_keeps_the_nearest_server(self):
        candidates = [candidate(name=str(i), distance=float(i), load=50) for i in range(50)]
        names = {c.name for c in bs.shortlist(candidates, 10)}
        self.assertIn("0", names)

    def test_keeps_the_quietest_server_even_when_distant(self):
        candidates = [candidate(name=str(i), distance=float(i), load=90) for i in range(50)]
        candidates[49].load = 1
        names = {c.name for c in bs.shortlist(candidates, 10)}
        self.assertIn("49", names, "a far but idle server should still be measured")

    def test_falls_back_to_proton_score_without_distances(self):
        candidates = [candidate(name=str(i)) for i in range(50)]
        for index, item in enumerate(candidates):
            item.proton_score = float(50 - index)
        names = {c.name for c in bs.shortlist(candidates, 5)}
        self.assertIn("49", names, "lowest Proton score should survive the cut")


# --- ranking -----------------------------------------------------------

class TestRank(unittest.TestCase):
    def test_faster_server_wins_at_equal_load(self):
        slow = candidate(name="slow", latency=300, load=50)
        fast = candidate(name="fast", latency=100, load=50)
        self.assertEqual([c.name for c in bs.rank([slow, fast])], ["fast", "slow"])

    def test_quieter_server_wins_at_equal_latency(self):
        busy = candidate(name="busy", latency=100, load=90)
        idle = candidate(name="idle", latency=100, load=10)
        self.assertEqual([c.name for c in bs.rank([busy, idle])], ["idle", "busy"])

    def test_latency_outweighs_load(self):
        # 60% latency vs 25% load: a big latency win should survive a full
        # load disadvantage.
        near_busy = candidate(name="near-busy", latency=100, load=95)
        far_idle = candidate(name="far-idle", latency=300, load=0)
        self.assertEqual([c.name for c in bs.rank([far_idle, near_busy])],
                         ["near-busy", "far-idle"])

    def test_unreachable_servers_sort_last(self):
        reachable = candidate(name="up", latency=250, load=99)
        unreachable = candidate(name="down", latency=None, load=1)
        self.assertEqual([c.name for c in bs.rank([unreachable, reachable])], ["up", "down"])

    def test_unreachable_servers_have_no_rating(self):
        unreachable = candidate(name="down", latency=None)
        self.assertIsNone(bs.rank([unreachable])[0].rating)

    def test_rating_is_a_percentage(self):
        candidates = [
            candidate(name="a", latency=100, load=10, distance=1000),
            candidate(name="b", latency=200, load=50, distance=5000),
            candidate(name="c", latency=300, load=90, distance=9000),
        ]
        for result in bs.rank(candidates):
            self.assertGreaterEqual(result.rating, 0.0)
            self.assertLessEqual(result.rating, 100.0)

    def test_best_candidate_rates_100(self):
        candidates = [
            candidate(name="best", latency=100, load=10, distance=1000),
            candidate(name="worst", latency=300, load=90, distance=9000),
        ]
        results = bs.rank(candidates)
        self.assertEqual(results[0].name, "best")
        self.assertAlmostEqual(results[0].rating, 100.0, places=1)

    def test_single_candidate_does_not_divide_by_zero(self):
        results = bs.rank([candidate(name="only", latency=123, load=50, distance=10)])
        self.assertEqual(results[0].rating, 100.0)

    def test_extreme_load_is_capped(self):
        # Beyond the saturation point, load stops differentiating, so the
        # faster server must still win.
        saturated_fast = candidate(name="fast", latency=100, load=99)
        saturated_slow = candidate(name="slow", latency=280, load=96)
        self.assertEqual([c.name for c in bs.rank([saturated_slow, saturated_fast])],
                         ["fast", "slow"])

    def test_handles_an_empty_list(self):
        self.assertEqual(bs.rank([]), [])


class TestRankWithoutProbing(unittest.TestCase):
    def test_orders_by_distance_then_load(self):
        far = candidate(name="far", distance=9000, load=10)
        near_busy = candidate(name="near-busy", distance=100, load=90)
        near_idle = candidate(name="near-idle", distance=100, load=10)
        results = bs.rank_without_probing([far, near_busy, near_idle])
        self.assertEqual([c.name for c in results], ["near-idle", "near-busy", "far"])

    def test_servers_without_distance_sort_last(self):
        known = candidate(name="known", distance=9999)
        unknown = candidate(name="unknown", distance=None)
        results = bs.rank_without_probing([unknown, known])
        self.assertEqual([c.name for c in results], ["known", "unknown"])

    def test_clears_ratings_since_nothing_was_measured(self):
        item = candidate(name="a", distance=1.0)
        item.rating = 99.0
        self.assertIsNone(bs.rank_without_probing([item])[0].rating)


# --- probing -----------------------------------------------------------

class TestProbing(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server = await asyncio.start_server(
            lambda reader, writer: writer.close(), "127.0.0.1", 0
        )
        self.port = self.server.sockets[0].getsockname()[1]

    async def asyncTearDown(self):
        self.server.close()
        await self.server.wait_closed()

    async def test_reports_a_time_for_a_listening_port(self):
        elapsed = await bs._probe_once("127.0.0.1", self.port, timeout=2.0)
        self.assertIsNotNone(elapsed)
        self.assertGreaterEqual(elapsed, 0.0)

    async def test_returns_none_for_a_closed_port(self):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            closed_port = probe.getsockname()[1]
        self.assertIsNone(await bs._probe_once("127.0.0.1", closed_port, timeout=1.0))

    async def test_returns_none_for_an_unroutable_address(self):
        # 203.0.113.0/24 is TEST-NET-3: reserved for documentation, never
        # routed, so this exercises the timeout path without real traffic.
        self.assertIsNone(await bs._probe_once("203.0.113.1", 443, timeout=0.2))

    async def test_measure_marks_reachable_and_unreachable(self):
        original_port = bs.PROBE_PORT
        bs.PROBE_PORT = self.port
        try:
            up = candidate(name="up")
            up.entry_ip = "127.0.0.1"
            down = candidate(name="down")
            down.entry_ip = "203.0.113.1"
            results = await bs.measure([up, down], rounds=1, timeout=0.3, refine=2)
        finally:
            bs.PROBE_PORT = original_port

        self.assertEqual(results[0].name, "up")
        self.assertTrue(results[0].reachable)
        self.assertFalse(results[1].reachable)

    async def test_a_probe_never_worsens_a_measurement(self):
        """The refine pass must not bury what the sweep already measured.

        measure() probes twice, and the second pass is the one under
        contention from nothing else - but it can still time out. Letting
        it assign unconditionally meant a slow sample replaced a fast one,
        and a pass that timed out entirely marked a server we had already
        reached as unreachable, dropping it out of the ranking.
        """
        target = candidate(name="target")
        target.latency_ms = 99.0
        semaphore = asyncio.Semaphore(1)

        async def probe_returning(value):
            async def fake(*_args, **_kwargs):
                return value
            original, bs._probe_once = bs._probe_once, fake
            try:
                await bs._probe_candidate(target, 2, 0.1, semaphore)
            finally:
                bs._probe_once = original

        await probe_returning(None)
        self.assertEqual(target.latency_ms, 99.0, "a failed pass erased a good one")

        await probe_returning(250.0)
        self.assertEqual(target.latency_ms, 99.0, "a slower pass replaced a faster one")

        await probe_returning(40.0)
        self.assertEqual(target.latency_ms, 40.0, "a faster pass was ignored")


# --- output ------------------------------------------------------------

class TestRendering(unittest.TestCase):
    def setUp(self):
        self.results = bs.rank([
            candidate(name="SG-FREE#2", latency=100, load=77, distance=6300),
            candidate(name="NL-FREE#15", latency=260, load=55, distance=16600),
        ])

    def test_table_lists_every_server(self):
        table = bs.render_table(self.results, origin=(-33.87, 151.22))
        self.assertIn("SG-FREE#2", table)
        self.assertIn("NL-FREE#15", table)
        self.assertIn("RATING", table)

    def test_table_reports_the_origin(self):
        table = bs.render_table(self.results, origin=(-33.87, 151.22))
        self.assertIn("-33.87", table)

    def test_table_copes_with_no_results(self):
        self.assertIn("No servers", bs.render_table([], origin=None))

    def test_table_marks_unmeasured_values(self):
        table = bs.render_table([candidate(name="X")], origin=None)
        self.assertIn("--", table)

    def test_json_round_trips(self):
        payload = json.loads(bs.render_json(self.results, origin=(-33.87, 151.22)))
        self.assertEqual(payload["origin"]["lat"], -33.87)
        self.assertEqual([r["name"] for r in payload["results"]], ["SG-FREE#2", "NL-FREE#15"])

    def test_json_publishes_the_weights(self):
        payload = json.loads(bs.render_json(self.results, origin=None))
        self.assertAlmostEqual(
            payload["weights"]["latency"]
            + payload["weights"]["load"]
            + payload["weights"]["distance"],
            1.0,
            places=6,
        )


# --- command line ------------------------------------------------------

class TestMain(unittest.TestCase):
    """End-to-end runs against a fixture file, with probing switched off."""

    SYDNEY = (-33.87, 151.22)

    def setUp(self):
        # Pin the origin so the expected ordering does not depend on where
        # the machine running the tests happens to be.
        original_coordinates = bs.local_coordinates
        bs.local_coordinates = lambda: self.SYDNEY
        self.addCleanup(setattr, bs, "local_coordinates", original_coordinates)

        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.path = Path(self.tempdir.name) / "serverlist.json"
        self.path.write_text(json.dumps(make_serverlist([
            make_logical(name="SG-FREE#2", country="SG", city="Singapore",
                         lat=1.29, long=103.85, load=77, score=5.0),
            make_logical(name="JP-FREE#9", country="JP", city="Tokyo",
                         lat=35.68, long=139.69, load=81, score=5.0),
            make_logical(name="NL-FREE#15", country="NL", city="Amsterdam",
                         lat=52.37, long=4.89, load=55, score=4.93),
            make_logical(name="NL-PLUS#1", country="NL", city="Amsterdam",
                         tier=2, lat=52.37, long=4.89),
        ])))

    def run_main(self, *argv):
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = bs.main(["--serverlist", str(self.path), "--no-probe", *argv])
        return code, buffer.getvalue()

    def test_ranks_by_distance_without_probing(self):
        code, output = self.run_main("--format", "names")
        self.assertEqual(code, 0)
        # From Sydney: Singapore, then Tokyo, then Amsterdam.
        self.assertEqual(output.split(), ["SG-FREE#2", "JP-FREE#9", "NL-FREE#15"])

    def test_omits_servers_above_the_account_tier(self):
        _, output = self.run_main("--format", "names")
        self.assertNotIn("NL-PLUS#1", output)

    def test_country_filter(self):
        _, output = self.run_main("--format", "names", "--country", "JP")
        self.assertEqual(output.split(), ["JP-FREE#9"])

    def test_limit_caps_the_results(self):
        _, output = self.run_main("--format", "names", "--limit", "1")
        self.assertEqual(output.split(), ["SG-FREE#2"])

    def test_json_output_is_valid(self):
        _, output = self.run_main("--format", "json")
        self.assertEqual(len(json.loads(output)["results"]), 3)

    def test_reports_a_missing_server_list(self):
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = bs.main(["--serverlist", "/nonexistent/serverlist.json"])
        self.assertEqual(code, 2)

    def test_reports_an_empty_match(self):
        code, _ = self.run_main("--country", "ZZ")
        self.assertEqual(code, 1)

    def test_names_omit_servers_that_did_not_answer(self):
        """`pvpn up` spends a connect attempt on each name it is given.

        rank() keeps unreachable servers on purpose, so a table can show
        that one was tried and did not answer. A connect list is not a
        report, and the filter has to happen before the limit or asking for
        two connectable names quietly returns one.
        """
        buffer = io.StringIO()

        async def only_singapore_answers(candidates, **_kwargs):
            for candidate in candidates:
                candidate.latency_ms = 40.0 if candidate.country == "SG" else None
            return bs.rank(candidates)

        original, bs.measure = bs.measure, only_singapore_answers
        try:
            with redirect_stdout(buffer):
                bs.main(["--serverlist", str(self.path), "--format", "names", "--limit", "2"])
        finally:
            bs.measure = original

        self.assertEqual(buffer.getvalue().split(), ["SG-FREE#2"])

    def test_names_fall_back_when_nothing_answers(self):
        """A blocked port 443 must not leave `pvpn up` with nothing to try."""
        buffer = io.StringIO()

        async def nothing_answers(candidates, **_kwargs):
            return bs.rank(candidates)

        original, bs.measure = bs.measure, nothing_answers
        try:
            with redirect_stdout(buffer):
                bs.main(["--serverlist", str(self.path), "--format", "names", "--limit", "2"])
        finally:
            bs.measure = original

        self.assertEqual(buffer.getvalue().split(), ["SG-FREE#2", "JP-FREE#9"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
