# Scenario file parser.
# Format (whitespace-separated, # comments):
#   portfolio NAME CAPITAL
#   equity SYMBOL WEIGHT MU SIGMA SECTOR
#   bond   SYMBOL WEIGHT MU SIGMA DURATION
#   sim    PATHS YEARS STEPS_PER_YEAR SEED
#
# Exercises: file I/O, with, generators, match, walrus, exceptions,
# assert, string methods, Optional, raise hierarchy, int()/float() from text.

from risksim.models import Asset, Equity, Bond, Portfolio, SimConfig


def _skip_blank(line: str) -> bool:
    s = line.strip()
    return s == "" or s.startswith("#")


def parse_scenario(path: str) -> tuple[Portfolio, SimConfig]:
    portfolio: Portfolio | None = None
    config: SimConfig | None = None
    line_no = 0

    with open(path, "r") as f:
        for raw in f.readlines():
            line_no = line_no + 1
            if _skip_blank(raw):
                continue
            parts = raw.strip().split()
            if len(parts) == 0:
                continue
            kind = parts[0].lower()
            try:
                match kind:
                    case "portfolio":
                        if len(parts) != 3:
                            raise ValueError("portfolio NAME CAPITAL")
                        empty: list[Asset] = []
                        portfolio = Portfolio(parts[1], float(parts[2]), empty)
                    case "equity":
                        if portfolio is None:
                            raise ValueError("equity before portfolio")
                        if len(parts) != 6:
                            raise ValueError("equity SYM W MU SIG SECTOR")
                        portfolio.add(
                            Equity(
                                parts[1],
                                float(parts[2]),
                                float(parts[3]),
                                float(parts[4]),
                                parts[5],
                            )
                        )
                    case "bond":
                        if portfolio is None:
                            raise ValueError("bond before portfolio")
                        if len(parts) != 6:
                            raise ValueError("bond SYM W MU SIG DUR")
                        portfolio.add(
                            Bond(
                                parts[1],
                                float(parts[2]),
                                float(parts[3]),
                                float(parts[4]),
                                float(parts[5]),
                            )
                        )
                    case "asset":
                        if portfolio is None:
                            raise ValueError("asset before portfolio")
                        if len(parts) != 5:
                            raise ValueError("asset SYM W MU SIG")
                        portfolio.add(
                            Asset(
                                parts[1],
                                float(parts[2]),
                                float(parts[3]),
                                float(parts[4]),
                            )
                        )
                    case "sim":
                        if len(parts) != 5:
                            raise ValueError("sim PATHS YEARS SPY SEED")
                        config = SimConfig(
                            int(parts[1]),
                            int(parts[2]),
                            int(parts[3]),
                            int(parts[4]),
                        )
                    case _:
                        raise ValueError(f"unknown directive {kind!r}")
            except ValueError as e:
                raise ValueError(f"{path}:{line_no}: {e}")

    if portfolio is None:
        raise ValueError(f"{path}: missing portfolio line")
    if config is None:
        raise ValueError(f"{path}: missing sim line")
    if not portfolio:
        raise ValueError(f"{path}: empty portfolio")
    assert config.paths > 0, "paths must be positive"
    assert config.years > 0, "years must be positive"
    assert config.steps_per_year > 0, "steps_per_year must be positive"
    return (portfolio, config)


def load_directive_lines(path: str) -> list[str]:
    # Non-comment logical lines from the scenario file.
    out: list[str] = []
    with open(path, "r") as f:
        for raw in f.readlines():
            if _skip_blank(raw):
                continue
            out.append(raw.strip())
    return out


def count_lines_gen(path: str):
    # Tiny generator so the program still exercises yield.
    lines = load_directive_lines(path)
    i = 0
    n = len(lines)
    while i < n:
        yield i
        i = i + 1
