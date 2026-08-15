# Domain model: assets, portfolio, config, results.
# Exercises: classes, single inheritance, super(), classmethod, staticmethod,
# property, bound methods, __str__/__repr__, __len__/__bool__/__contains__,
# isinstance peels, Optional/unions, closed-world fields.

class Asset:
    def __init__(self, symbol: str, weight: float, mu: float, sigma: float):
        self.symbol = symbol
        self.weight = weight
        self.mu = mu
        self.sigma = sigma

    def __str__(self) -> str:
        return f"{self.symbol}(w={self.weight:.2f}, μ={self.mu:.3f}, σ={self.sigma:.3f})"

    def __repr__(self) -> str:
        return f"Asset({self.symbol!r})"

    @staticmethod
    def validate_weight(w: float) -> bool:
        return w > 0.0 and w <= 1.0

    def annual_vol(self) -> float:
        return self.sigma


class Equity(Asset):
    def __init__(self, symbol: str, weight: float, mu: float, sigma: float, sector: str):
        super().__init__(symbol, weight, mu, sigma)
        self.sector = sector

    def __str__(self) -> str:
        base = super().__str__()
        return f"{base}[{self.sector}]"


class Bond(Asset):
    def __init__(self, symbol: str, weight: float, mu: float, sigma: float, duration: float):
        super().__init__(symbol, weight, mu, sigma)
        self.duration = duration

    def __str__(self) -> str:
        return f"{super().__str__()} dur={self.duration:.1f}y"


class Portfolio:
    def __init__(self, name: str, capital: float, assets: list[Asset]):
        self.name = name
        self.capital = capital
        # List fields must be initialized from a typed list value (not bare []).
        self.assets = assets

    def add(self, a: Asset) -> None:
        if not Asset.validate_weight(a.weight):
            raise ValueError(f"invalid weight for {a.symbol}")
        self.assets.append(a)

    def __len__(self) -> int:
        return len(self.assets)

    def __bool__(self) -> bool:
        return len(self.assets) > 0 and self.capital > 0.0

    def __contains__(self, symbol: str) -> bool:
        for a in self.assets:
            if a.symbol == symbol:
                return True
        return False

    def __str__(self) -> str:
        return f"Portfolio({self.name!r}, capital={self.capital:.2f}, n={len(self)})"

    @property
    def total_weight(self) -> float:
        s = 0.0
        for a in self.assets:
            s = s + a.weight
        return s

    def weight_map(self) -> dict[str, float]:
        d: dict[str, float] = {}
        for a in self.assets:
            d[a.symbol] = a.weight
        return d

    def symbols(self) -> list[str]:
        return [a.symbol for a in self.assets]

    def normalized(self) -> Portfolio:
        # Return a copy with weights renormalized to sum to 1.
        tw = self.total_weight
        if tw <= 0.0:
            raise ValueError("portfolio has zero total weight")
        empty: list[Asset] = []
        out = Portfolio(self.name, self.capital, empty)
        for a in self.assets:
            w = a.weight / tw
            if isinstance(a, Equity):
                out.add(Equity(a.symbol, w, a.mu, a.sigma, a.sector))
            elif isinstance(a, Bond):
                out.add(Bond(a.symbol, w, a.mu, a.sigma, a.duration))
            else:
                out.add(Asset(a.symbol, w, a.mu, a.sigma))
        return out

    @classmethod
    def balanced(cls, name: str, capital: float) -> Portfolio:
        empty: list[Asset] = []
        p = cls(name, capital, empty)
        p.add(Equity("SPY", 0.60, 0.08, 0.16, "broad"))
        p.add(Bond("AGG", 0.40, 0.03, 0.05, 6.5))
        return p


class SimConfig:
    def __init__(self, paths: int, years: int, steps_per_year: int, seed: int):
        self.paths = paths
        self.years = years
        self.steps_per_year = steps_per_year
        self.seed = seed

    @property
    def steps(self) -> int:
        return self.years * self.steps_per_year

    @property
    def dt(self) -> float:
        return 1.0 / float(self.steps_per_year)

    def __str__(self) -> str:
        return f"SimConfig(paths={self.paths}, years={self.years}, spy={self.steps_per_year}, seed={self.seed})"


class RunResult:
    def __init__(
        self,
        terminals: list[float],
        mean: float,
        median: float,
        var95: float,
        cvar95: float,
        p05: float,
        p95: float,
        best: float,
        worst: float,
        elapsed_s: float,
        paths: int,
        steps: int,
    ):
        self.terminals = terminals
        self.mean = mean
        self.median = median
        self.var95 = var95
        self.cvar95 = cvar95
        self.p05 = p05
        self.p95 = p95
        self.best = best
        self.worst = worst
        self.elapsed_s = elapsed_s
        self.paths = paths
        self.steps = steps

    def summary_lines(self) -> list[str]:
        lines: list[str] = [
            f"paths={self.paths} steps/path={self.steps}",
            f"mean_terminal={self.mean:.4f}",
            f"median_terminal={self.median:.4f} (from sample)",
            f"p05={self.p05:.4f} p95={self.p95:.4f} (from sample)",
            f"VaR_95={self.var95:.4f} CVaR_95={self.cvar95:.4f}",
            f"best={self.best:.4f} worst={self.worst:.4f}",
            f"sample_n={len(self.terminals)} (capped; full paths streamed)",
        ]
        if self.elapsed_s > 0.0:
            lines.append(f"elapsed_s={self.elapsed_s:.6f}")
            lines.append(f"paths_per_s={float(self.paths) / self.elapsed_s:.1f}")
        return lines
