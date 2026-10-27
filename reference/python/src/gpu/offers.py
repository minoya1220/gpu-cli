"""Vast offer search, the known GPU model catalog, and their display labels."""

from __future__ import annotations

import argparse
import os
import re
import tempfile
import threading
from importlib.resources import files
from pathlib import Path
from typing import Any

NO_OFFERS = "No matching offers found; try a different model, GPU count, reliability, or disk size"
CATEGORY_ORDER = [
    "Datacenter Blackwell/Hopper",
    "Datacenter Ampere/Ada/older",
    "Workstation/Pro",
    "Consumer RTX 50 series",
    "Consumer RTX 40 series",
    "Consumer RTX 30 and older",
    "Other",
]
# Offer searches can run on several picker threads at once.
_cache_lock = threading.Lock()


class OfferSearch:
    """Vast offer queries for one `gpu up` run; the mode is chosen later."""

    def __init__(self, vast: Any, args: argparse.Namespace, cfg: dict[str, Any]):
        self._vast = vast
        self._bid = args.interruptible
        self._max_price = args.max_price
        choosing_offer = not args.yes and args.offer is None
        # An interactive offer choice shows unverified hosts but never deverified ones.
        self._hide_deverified = choosing_offer and not args.unverified
        verified = "" if args.unverified or choosing_offer else " verified=true"
        reliability = args.reliability if args.reliability is not None else cfg["reliability"]
        self._base = f"external=false rentable=true{verified} num_gpus={args.num_gpus} reliability>={reliability}"
        self._disk = {"container": args.disk or cfg["disk"], "vm": args.disk or cfg["vm_disk"]}

    def query(self, mode: str, gpu: str | None = None) -> str:
        query = f"{self._base} disk_space>={self._disk[mode]}"
        if mode == "vm":
            query += " vms_enabled=true"
        if gpu:
            query += " gpu_name=" + gpu.replace(" ", "_")
        if self._max_price is not None:
            query += f" {'min_bid' if self._bid else 'dph'}<={self._max_price}"
        return query

    def offers(self, mode: str, gpu: str | None = None) -> list[dict[str, Any]]:
        """Matching offers, cheapest first. Blocks on the Vast CLI."""
        items = self._vast.offers(self.query(mode, gpu), bid=self._bid)
        update_gpu_cache(items)
        if self._max_price is not None:
            items = [o for o in items if offer_price(o) <= self._max_price]
        if self._hide_deverified:
            items = [o for o in items if o.get("verification") != "deverified"]
        return sorted(items, key=offer_price)

    def stats(self, mode: str) -> dict[str, tuple[float, int]]:
        """Cheapest hourly price and offer count per GPU model. Blocks on the Vast CLI."""
        return gpu_stats(self.offers(mode))


def offer_price(offer: dict[str, Any]) -> float:
    return float(offer.get("dph_total", offer.get("dph", float("inf"))))


def gpu_stats(offers: list[dict[str, Any]]) -> dict[str, tuple[float, int]]:
    stats: dict[str, tuple[float, int]] = {}
    for offer in offers:
        cheapest, count = stats.get(offer["gpu_name"], (float("inf"), 0))
        stats[offer["gpu_name"]] = (min(cheapest, offer_price(offer)), count + 1)
    return stats


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def offer_label(offer: dict[str, Any]) -> str:
    return (
        f"id={offer['id']} {offer['gpu_name']} x{offer['num_gpus']} ${offer_price(offer):.3f}/hr"
        f" reliability={float(offer.get('reliability', 0)):.3f}"
        f" location={offer.get('geolocation', '?')}"
        f" verification={offer.get('verification', 'unknown')}"
    )


def offer_rows(offers: list[dict[str, Any]]) -> list[str]:
    """Aligned one-line offer rows for a single GPU model (the picker title names it)."""
    prices = [f"${offer_price(offer):.3f}/hr" for offer in offers]
    checks = [str(offer.get("verification", "unknown")) for offer in offers]
    ids = [f"id={offer['id']}" for offer in offers]
    price_width = max(map(len, prices), default=0)
    check_width = max(map(len, checks), default=0)
    id_width = max(map(len, ids), default=0)
    return [
        f"{price:>{price_width}}  {float(offer.get('reliability', 0)):6.1%} reliable"
        f"  {check:<{check_width}}  {offer_id:<{id_width}}  {offer.get('geolocation', '?')}"
        for offer, price, check, offer_id in zip(offers, prices, checks, ids)
    ]


def model_label(model: str, stats: dict[str, tuple[float, int]]) -> str:
    if model not in stats:
        return model
    price, count = stats[model]
    return f"{model}  ${price:.3f}/hr ({plural(count, 'offer')})"


def category_label(category: str, models: list[str], stats: dict[str, tuple[float, int]]) -> str:
    priced = [model for model in models if model in stats]
    if not priced:
        return f"{category} ({plural(len(models), 'model')})"
    cheapest = min(stats[model][0] for model in priced)
    return f"{category} ({plural(len(priced), 'model')}, from ${cheapest:.3f}/hr)"


def gpu_category(model: str) -> str:
    name = re.sub(r"[^a-z0-9]+", "", model.casefold())
    if name.startswith("q" + "rtx"):
        name = "quadro" + name[1:]
    if any(token in name for token in ("b200", "b300", "h100", "h200", "h800", "h20")):
        return "Datacenter Blackwell/Hopper"
    if name.startswith(("quadro", "rtxpro", "rtxa", "rtx6000d")) or re.match(r"rtx\d+ada", name):
        return "Workstation/Pro"
    if any(
        token in name
        for token in ("a100", "a800", "a40", "a10", "a30", "l4", "l40", "t4", "p100", "p4", "v100", "tesla")
    ):
        return "Datacenter Ampere/Ada/older"
    if name.startswith("rtx5"):
        return "Consumer RTX 50 series"
    if name.startswith("rtx4"):
        return "Consumer RTX 40 series"
    if name.startswith(("rtx3", "rtx2", "gtx", "titan")):
        return "Consumer RTX 30 and older"
    return "Other"


def group_by_category(models: list[str]) -> dict[str, list[str]]:
    """Models per category, in display order, omitting empty categories."""
    groups: dict[str, list[str]] = {category: [] for category in CATEGORY_ORDER}
    for model in models:
        groups[gpu_category(model)].append(model)
    return {category: members for category, members in groups.items() if members}


def known_gpu_models() -> list[str]:
    """Bundled model names merged with names seen in earlier offer searches."""
    bundled = files("gpu").joinpath("gpu_models.txt").read_text().splitlines()
    return sorted({name for name in bundled if valid_gpu_name(name)} | set(read_gpu_cache()))


def gpu_cache_path() -> Path:
    root = os.environ.get("XDG_CACHE_HOME")
    return (Path(root) if root else Path.home() / ".cache") / "gpu" / "gpu_models.txt"


def valid_gpu_name(value: object) -> bool:
    return (
        isinstance(value, str)
        and 2 <= len(value) <= 100
        and value == value.strip()
        and all(char.isalnum() or char in " .,+()/_-" for char in value)
    )


def read_gpu_cache(path: Path | None = None) -> list[str]:
    cache = path or gpu_cache_path()
    try:
        return sorted({name for name in cache.read_text().splitlines() if valid_gpu_name(name)})
    except OSError:
        return []


def update_gpu_cache(items: list[dict[str, Any]], path: Path | None = None) -> None:
    names = {item["gpu_name"] for item in items if valid_gpu_name(item.get("gpu_name"))}
    if not names:
        return
    cache = path or gpu_cache_path()
    with _cache_lock:
        names.update(read_gpu_cache(cache))
        try:
            cache.parent.mkdir(parents=True, exist_ok=True)
            fd, tmp = tempfile.mkstemp(dir=cache.parent, suffix=".tmp")
        except OSError:
            return
        # Replace atomically so a concurrent reader never sees a partial file.
        try:
            with os.fdopen(fd, "w") as file:
                file.write("\n".join(sorted(names)) + "\n")
            os.replace(tmp, cache)
        except OSError:
            Path(tmp).unlink(missing_ok=True)
