#!/usr/bin/env python3
"""Regenerate resources/trackers.json, the browser's built-in tracker blocker.

Source: Disconnect's tracking-protection lists (the lists Firefox's Enhanced
Tracking Protection uses). License: CC BY-NC-SA 4.0, so non-commercial use only.

A tracker domain is blocked only when loaded as a third party on a site its
owner doesn't run. For example, Facebook's servers are blocked on news sites
but allowed on messenger.com, because the entity list says Meta owns both.

Usage: python3 scripts/update-trackers.py
"""

import json
import pathlib
import re
import struct
import subprocess
import zlib

BASE = "https://raw.githubusercontent.com/disconnectme/disconnect-tracking-protection/master/"
# Categories that exist only to follow or profile people. "Content" (embedded
# videos, CDNs), "Anti-fraud" and "ConsentManagers" are left alone: blocking
# them breaks sites.
BLOCKED = [
    "Advertising",
    "Analytics",
    "Social",
    "FingerprintingInvasive",
    "FingerprintingGeneral",
    "Cryptomining",
    "Email",
    "EmailAggressive",
]
# Raw DEFLATE with an 8-byte little-endian length header. The JSON is ~6.5 MB
# (each rule repeats its owner's site list) but compresses ~25x; the browser
# inflates it with macOS's built-in libcompression on first launch.
OUT = pathlib.Path(__file__).resolve().parent.parent / "resources" / "trackers.bin"


def fetch(name):
    # curl uses the system certificate store; python.org builds of Python often have none.
    out = subprocess.run(["curl", "-sSfL", "--max-time", "60", BASE + name], check=True, capture_output=True)
    return json.loads(out.stdout)


def tracker_domains(services):
    """Every domain listed under a blocked category."""
    domains = set()
    for category in BLOCKED:
        for company in services["categories"].get(category, []):
            for sites in company.values():
                for value in sites.values():
                    # Entries also carry flags like {"dnt": "eff"}; only lists are domains.
                    if isinstance(value, list):
                        domains.update(d.lower() for d in value)
    return domains


HOSTNAME = re.compile(r"[a-z0-9.-]+")


def owner_sites(entities):
    """Map each resource domain to the sites run by the same company."""
    owners = {}
    for entity in entities["entities"].values():
        resources = [r.lower() for r in entity.get("resources", [])]
        # WebKit only accepts lowercase ASCII hostnames here; the lists also
        # contain a few URL paths ("yandex.ru/clck/click"), which aren't sites.
        sites = {d.lower() for d in entity.get("properties", []) + entity.get("resources", [])}
        sites = {d for d in sites if HOSTNAME.fullmatch(d)}
        for resource in resources:
            owners.setdefault(resource, set()).update(sites)
    return owners


def rule(entry, exempt_sites):
    # An entry is a domain ("doubleclick.net") or a domain plus path prefix
    # ("google.com/pagead/1p-user-list"). WebKit's url-filter is a restricted
    # regex (no "|"); this matches the domain and any subdomain, any scheme.
    host, slash, path = entry.partition("/")
    pattern = "^[^:]+://+([^:/]+\\.)?" + re.escape(host).replace("\\-", "-")
    pattern += "(:[0-9]+)?/" + re.escape(path).replace("\\-", "-") if slash else "[:/]"
    trigger = {"url-filter": pattern, "load-type": ["third-party"]}
    if exempt_sites:
        # "*site" means the site and its subdomains.
        trigger["unless-domain"] = sorted("*" + s for s in exempt_sites)
    return {"trigger": trigger, "action": {"type": "block"}}


def main():
    services, entities = fetch("services.json"), fetch("entities.json")
    owners = owner_sites(entities)
    domains = sorted(tracker_domains(services))
    rules = [rule(d, owners.get(d, {d.partition("/")[0]})) for d in domains]
    data = json.dumps(rules, separators=(",", ":")).encode()
    deflate = zlib.compressobj(9, zlib.DEFLATED, -15)
    packed = struct.pack("<Q", len(data)) + deflate.compress(data) + deflate.flush()
    OUT.parent.mkdir(exist_ok=True)
    OUT.write_bytes(packed)
    print(f"{len(rules)} rules, {len(data)} bytes JSON, {len(packed)} bytes packed -> {OUT}")


if __name__ == "__main__":
    main()
