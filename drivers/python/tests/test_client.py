"""Runs the Python driver against a real server.

    node drivers/testing/server.mjs python -m unittest discover -s drivers/python/tests -t drivers/python

HEXDB_URL and HEXDB_API_KEY name the server (set by server.mjs).
"""

import os
import threading
import time
import unittest

from hexdb import HexDB, HexDBError

URL = os.environ.get("HEXDB_URL")
KEY = os.environ.get("HEXDB_API_KEY")


@unittest.skipUnless(URL and KEY, "set HEXDB_URL and HEXDB_API_KEY (or run through drivers/testing/server.mjs)")
class ClientTest(unittest.TestCase):
    def setUp(self):
        self.db = HexDB(URL, KEY)
        self.prefix = f"py{int(time.time() * 1000) % 10_000_000}"

    def tess(self, name):
        return self.db.tessellation(f"{self.prefix}_{name}")

    def test_documents(self):
        notes = self.tess("notes")
        doc = notes.insert({"title": "hello"})
        self.assertEqual(len(doc["id"]), 26)
        self.assertEqual(notes.get(doc["id"])["title"], "hello")
        self.assertEqual(notes.patch(doc["id"], {"title": "hi"})["title"], "hi")
        self.assertEqual(notes.replace(doc["id"], {"body": "x"})["body"], "x")
        self.assertTrue(notes.delete(doc["id"]))
        self.assertIsNone(notes.get(doc["id"]))
        self.assertFalse(notes.delete(doc["id"]))

    def test_queries_and_upserts(self):
        orders = self.tess("orders")
        ids = orders.insert_many([{"n": i, "total": i * 10, "status": "paid" if i % 2 else "new"} for i in range(20)])
        self.assertEqual(len(ids), 20)
        page = orders.query(filter={"status": "paid"}, sort="-total", limit=2, fields=["total"])
        self.assertEqual(page["total"], 10)
        self.assertEqual([d["total"] for d in page["documents"]], [190, 170])
        self.assertEqual(orders.count({"total": {"$lt": 50}}), 5)
        self.assertEqual(sum(1 for _ in orders.iterate(page_size=6)), 20)
        agg = orders.aggregate(["status"], {"sum": {"$sum": "total"}})
        self.assertEqual(len(agg["rows"]), 2)
        result = orders.upsert("n", [{"n": 0, "status": "void"}, {"n": 100}])
        self.assertEqual((result["inserted"], result["replaced"]), (1, 1))
        self.assertEqual(result["ids"][0], ids[0])
        self.assertEqual(orders.update_where({"status": "void"}, {"refunded": True})["modified"], 1)

    def test_transactions_and_errors(self):
        ledger = f"{self.prefix}_ledger"
        op = [{"op": "insert", "tessellation": ledger, "data": {"amount": 5}}]
        self.db.transaction(op, idempotency_key=f"{self.prefix}-tx")
        self.db.transaction(op, idempotency_key=f"{self.prefix}-tx")
        self.assertEqual(self.db.tessellation(ledger).count(), 1)
        with self.assertRaises(HexDBError) as caught:
            self.db.tessellation(ledger).query(filter={"$bogus": 1})
        self.assertEqual(caught.exception.status, 400)
        with self.assertRaises(HexDBError) as caught:
            HexDB(URL).tessellation(ledger).count()
        self.assertEqual((caught.exception.status, caught.exception.code), (401, "unauthorized"))

    def test_graphql_and_changes(self):
        feed = f"{self.prefix}_feed"
        self.tess("feed").insert({"seed": True})
        data = self.db.graphql("query($t: String!) { count(tessellation: $t) }", {"t": feed})
        self.assertEqual(data["count"], 1)
        received = []
        done = threading.Event()

        def follow():
            for change in self.db.changes(tessellation=feed, stop=done.is_set):
                received.append(change)
                done.set()

        thread = threading.Thread(target=follow, daemon=True)
        thread.start()
        time.sleep(0.5)
        doc = self.tess("feed").insert({"x": 1})
        thread.join(timeout=35)
        self.assertEqual(received[0]["id"], doc["id"])

    def test_streams(self):
        name = f"{self.prefix}-events"
        self.db.request("POST", "/streams", {"name": name})
        events = self.db.stream(name)
        offsets = events.publish({"payload": {"n": 1}}, {"payload": {"n": 2}, "key": "k"})
        self.assertEqual(len(offsets), 2)
        self.assertEqual([m["payload"]["n"] for m in events.read()["messages"]], [1, 2])
        got = []
        events.consume("workers", lambda m: got.append(m["payload"]["n"]), stop=lambda: len(got) >= 2)
        self.assertEqual(got, [1, 2])
        self.assertEqual(events.read(group="workers")["messages"], [])


if __name__ == "__main__":
    unittest.main()
