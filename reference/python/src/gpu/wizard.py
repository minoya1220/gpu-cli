"""The keyboard-driven `gpu up` flow: repository, mode, GPU, and offer screens."""

from __future__ import annotations

from dataclasses import dataclass, replace
from typing import Any

from gpu.offers import (
    NO_OFFERS,
    OfferSearch,
    category_label,
    group_by_category,
    model_label,
    offer_rows,
)
from gpu.picker import Job, Option, Screen, pick
from gpu.repos import CLONE_URL, NO_REPO, load_cached_repos, resolve_repo

MODES = ["container", "vm"]


@dataclass(frozen=True)
class Picks:
    """Launch choices; None means not chosen yet."""

    repo: str | None = None
    mode: str | None = None
    gpu: str | None = None
    offer: dict[str, Any] | None = None


@dataclass(frozen=True)
class _Category:
    name: str


class LaunchWizard:
    """Builds the screens for whatever `Picks` are still missing.

    Each screen's `select` returns the screen for the next missing pick, so
    going back is just popping the picker's stack: nothing is refetched and
    earlier choices stay intact.
    """

    def __init__(
        self,
        *,
        repos: Job[list[str]],
        search: OfferSearch,
        models: list[str],
        recent_gpus: list[str],
        choose_offer: bool,
        owner: str = "minoya1220",
    ):
        self._repos = repos
        self._owner = owner
        self._cached = load_cached_repos(owner)
        self._chosen_cached: str | None = None
        self._entered_repo_screen = False
        self._invalidated = False
        self._selected_repo = False
        self._search = search
        self._models = models
        self._categories = group_by_category(models)
        self._recent_gpus = [gpu for gpu in recent_gpus if gpu in models][:5]
        self._choose_offer = choose_offer
        self._prices: dict[str, Job[dict[str, tuple[float, int]]]] = {}

    def run(self, picks: Picks, *, input=None, output=None) -> Picks:
        first = self.next(picks)
        return pick(first, input=input, output=output) if isinstance(first, Screen) else first

    def next(self, picks: Picks) -> Screen | Picks:
        if picks.repo is None:
            return self._repo_screen(picks)
        if picks.mode is None:
            return self._mode_screen(picks)
        if picks.gpu is None:
            return self._gpu_screen(picks)
        if picks.offer is None and self._choose_offer:
            return self._offer_screen(picks)
        if self._entered_repo_screen:
            if self._repos.state == "failed":
                self._chosen_cached = None
                return self._repo_screen(Picks(), notice=self._repos.error)
            if self._repos.state != "done":
                return Screen(
                    "Checking repositories", options=list, select=lambda _: picks,
                    load=self._repos, loading="Checking repositories...",
                    on_settled=lambda: self._sync_result() or (None if self._invalidated else picks),
                )
            if self._chosen_cached is not None and self._chosen_cached not in (self._repos.result or []):
                return self._repo_screen(Picks(), notice=f"Repository not available or deleted: {self._chosen_cached}")
        return picks

    def _sync_result(self) -> Screen | None:
        if self._selected_repo:
            if self._repos.state == "failed":
                self._chosen_cached = None
                self._invalidated = True
                return self._repo_screen(Picks(), notice=self._repos.error)
            if self._repos.state == "done" and self._chosen_cached is not None and self._chosen_cached not in (self._repos.result or []):
                name = self._chosen_cached
                self._chosen_cached = None
                self._invalidated = True
                return self._repo_screen(Picks(), notice=f"Repository not available or deleted: {name}")
        return None

    def _repo_screen(self, picks: Picks, notice: str = "") -> Screen:
        self._entered_repo_screen = True

        def options() -> list[Option]:
            return [
                Option(NO_REPO, "No repository"),
                Option(CLONE_URL, "Clone from URL..."),
                *(Option(name, name) for name in (self._repos.result if self._repos.state == "done" else self._cached) or []),
            ]

        def select(repo: str) -> Screen | Picks:
            if repo == CLONE_URL:
                return self._url_screen(picks)
            self._invalidated = False
            self._selected_repo = True
            self._chosen_cached = repo if repo not in (NO_REPO, CLONE_URL) else None
            return self.next(replace(picks, repo=repo))

        return Screen(
            "Repository", options=options, select=select,
            load=self._repos, load_required=False, wait_initial=self._cached is None,
            loading="Loading repositories...", notice=notice,
            on_settled=self._sync_result,
        )

    def _url_screen(self, picks: Picks) -> Screen:
        def validate(value: str) -> None:
            if not value:
                raise ValueError("Enter a repository, or `/← back to choose No repository")
            resolve_repo(value, self._owner)

        def select(repo: str) -> Screen | Picks:
            self._selected_repo = True
            self._chosen_cached = None
            return self.next(replace(picks, repo=repo))

        return Screen(
            "Clone from URL", options=list,
            select=select,
            text_input=True, validate=validate, quick_select=False,
        )

    def _mode_screen(self, picks: Picks) -> Screen:
        return Screen(
            "Mode",
            options=lambda: [Option(mode, mode) for mode in MODES],
            select=lambda mode: self.next(replace(picks, mode=mode)),
        )

    def _gpu_screen(self, picks: Picks) -> Screen:
        prices = self._price_job(picks.mode)

        def options() -> list[Option]:
            stats = prices.result or {}
            return [
                *(Option(gpu, f"Recent: {model_label(gpu, stats)}") for gpu in self._recent_gpus),
                *(
                    Option(_Category(name), category_label(name, members, stats))
                    for name, members in self._categories.items()
                ),
            ]

        def select(value: str | _Category) -> Screen | Picks:
            if isinstance(value, _Category):
                return self._category_screen(picks, value.name)
            return self.next(replace(picks, gpu=value))

        return Screen(
            "GPU",
            options=options,
            select=select,
            search=lambda: [Option(gpu, model_label(gpu, prices.result or {})) for gpu in self._models],
            prices=prices,
            quick_select=False,  # digits belong to names such as "RTX 4090"
            empty="No known GPU models",
        )

    def _category_screen(self, picks: Picks, category: str) -> Screen:
        prices = self._price_job(picks.mode)
        return Screen(
            category,
            options=lambda: [
                Option(gpu, model_label(gpu, prices.result or {})) for gpu in self._categories[category]
            ],
            select=lambda gpu: self.next(replace(picks, gpu=gpu)),
            prices=prices,
            quick_select=False,
        )

    def _offer_screen(self, picks: Picks) -> Screen:
        # Offers are live market data, so every visit searches again.
        offers = Job(lambda: self._search.offers(picks.mode, picks.gpu)[:10])

        def options() -> list[Option]:
            found = offers.result or []
            return [Option(offer, row) for offer, row in zip(found, offer_rows(found))]

        return Screen(
            f"Offer: {picks.gpu}",
            options=options,
            select=lambda offer: self.next(replace(picks, offer=offer)),
            load=offers,
            loading=f"Searching {picks.gpu} offers...",
            empty=NO_OFFERS,
        )

    def _price_job(self, mode: str) -> Job[dict[str, tuple[float, int]]]:
        """One shared price search per mode, so F2 on any GPU screen prices them all."""
        if mode not in self._prices:
            self._prices[mode] = Job(lambda: self._search.stats(mode))
        return self._prices[mode]
