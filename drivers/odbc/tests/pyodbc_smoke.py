"""The ODBC driver through a real driver manager (unixODBC or Windows'),
used from Python with pyodbc:

    node drivers/testing/server.mjs python3 drivers/odbc/tests/pyodbc_smoke.py

HEXDB_URL and HEXDB_API_KEY name the server (server.mjs sets them).
HEXDB_ODBC_DRIVER is the driver library (default: target/debug/libhexdb_odbc.so,
or hexdb_odbc.dll on Windows). On unixODBC the script also registers the
driver and a DSN in temporary odbcinst.ini / odbc.ini files to test DSNs.
"""

import json
import os
import sys
import tempfile
import urllib.request

import pyodbc

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
URL = os.environ["HEXDB_URL"]
KEY = os.environ["HEXDB_API_KEY"]
DEFAULT_LIB = os.path.join(ROOT, "target", "debug", "hexdb_odbc.dll" if os.name == "nt" else "libhexdb_odbc.so")
LIB = os.environ.get("HEXDB_ODBC_DRIVER", DEFAULT_LIB)


# Register the driver and a DSN for unixODBC before it first reads its configuration.
REGISTERED = os.name != "nt"
if REGISTERED:
    folder = tempfile.mkdtemp()
    with open(os.path.join(folder, "odbcinst.ini"), "w") as f:
        f.write(f"[HexDB]\nDescription = HexDB ODBC driver\nDriver = {LIB}\n")
    with open(os.path.join(folder, "odbc.ini"), "w") as f:
        f.write(f"[hexdb-test]\nDriver = HexDB\nServer = {URL}\nApiKey = {KEY}\n")
    os.environ["ODBCSYSINI"] = folder
    os.environ["ODBCINI"] = os.path.join(folder, "odbc.ini")


def api(method, path, body=None):
    request = urllib.request.Request(URL + path, method=method, data=None if body is None else json.dumps(body).encode(),
                                     headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
    with urllib.request.urlopen(request) as response:
        return json.loads(response.read() or b"null")


def check(condition, message):
    if not condition:
        print("FAIL:", message)
        sys.exit(1)
    print("ok:", message)


api("POST", "/products/_bulk", [
    {"sku": f"p-{i}", "name": f"Product {i} ✓", "price": i * 1.5, "stock": i, "active": i % 2 == 0,
     "added": f"2026-02-{i + 1:02d}T10:00:00Z", "dims": {"w": i}}
    for i in range(25)
])

# Connection string with the library path (no registration needed on unixODBC).
connection = f"Driver={{{LIB}}};Server={URL};ApiKey={KEY};PageSize=7"
db = pyodbc.connect(connection, autocommit=True)
check(db.getinfo(pyodbc.SQL_DBMS_NAME) == "HexDB", "SQLGetInfo: DBMS name")
check(db.getinfo(pyodbc.SQL_DATA_SOURCE_READ_ONLY) in ("Y", True), "SQLGetInfo: read-only")  # pyodbc maps Y/N to bool

cursor = db.cursor()
cursor.execute("SELECT sku, price, stock, active, added, dims FROM products WHERE price >= ? AND name LIKE ? ORDER BY stock", 3, "Product%")
names = [d[0] for d in cursor.description]
check(names == ["sku", "price", "stock", "active", "added", "dims"], f"description names {names}")
check(cursor.description[1][1] is float and cursor.description[2][1] is int and cursor.description[3][1] is bool, "description types (float, int, bool)")
rows = cursor.fetchall()
check(len(rows) == 23, f"all pages fetched with PageSize=7 ({len(rows)} rows)")
check(rows[0].sku == "p-2" and rows[0].price == 3.0 and rows[0].stock == 2 and rows[0].active is True, "values convert")
check(json.loads(rows[0].dims) == {"w": 2}, "objects arrive as JSON text")

cursor.execute("SELECT name FROM products WHERE sku = ?", "p-5")
check(cursor.fetchone()[0] == "Product 5 ✓", "Unicode text")

cursor.execute("SELECT COUNT(*) AS n, AVG(price) AS avg_price FROM products WHERE active")
row = cursor.fetchone()
check(row.n == 13 and abs(row.avg_price - 18.0) < 1e-9, f"aggregates ({row.n}, {row.avg_price})")

cursor.execute("SELECT sku FROM products WHERE added >= {ts '2026-02-20 00:00:00'} ORDER BY sku")
check(len(cursor.fetchall()) == 6, "ODBC escape sequences")

tables = [t.table_name for t in cursor.tables()]
check("products" in tables, f"SQLTables {tables}")
columns = {c.column_name: c.type_name for c in cursor.columns(table="products")}
check(columns.get("id") == "WVARCHAR" and columns.get("price") == "DOUBLE" and columns.get("stock") == "BIGINT", f"SQLColumns {columns}")
keys = [k.column_name for k in cursor.primaryKeys("products")]
check(keys == ["id"], "SQLPrimaryKeys")

try:
    cursor.execute("UPDATE products SET price = 0")
    check(False, "writes are refused")
except pyodbc.Error as e:
    check(e.args[0] == "42000" and "read-only" in str(e), f"writes are refused ({e.args[0]})")
db.close()

# A DSN registered with the driver manager (set up before the first connection).
if REGISTERED:
    db = pyodbc.connect("DSN=hexdb-test", autocommit=True)
    check(db.cursor().execute("SELECT COUNT(*) FROM products").fetchone()[0] == 25, "DSN settings are read from odbc.ini")
    db.close()
    db = pyodbc.connect(f"Driver=HexDB;Server={URL};UID=admin;PWD={{Driver test passphrase 2026}}", autocommit=True)
    check(db.cursor().execute("SELECT 1 AS one").fetchone().one == 1, "registered driver name, UID/PWD sign-in")
    db.close()

print("pyodbc smoke test passed")
