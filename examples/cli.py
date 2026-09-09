"""A command-line argument parser, and a small program built on it.

Everything here is ordinary Python that both engines run. The pieces a CLI
needs and that this exercises: keyword-only parameters for the option spec,
a dispatch table of module-level functions, `Callable` as a stored type,
`object` columns for parsed values of mixed type, and errors that carry an
exit status rather than a traceback.

The argument vectors are written out rather than read from `sys.argv`, so the
output is the same on every run and can be compared between engines.
"""

from typing import Callable


class UsageError(Exception):
    """A problem with what the user typed, not with the program."""


class Option:
    def __init__(
        self,
        name: str,
        *,
        short: str = "",
        help: str = "",
        takes_value: bool = False,
        default: object = None,
        required: bool = False,
    ):
        self.name: str = name
        self.short: str = short
        self.help: str = help
        self.takes_value: bool = takes_value
        self.default: object = default
        self.required: bool = required

    def flag(self) -> str:
        if self.short == "":
            return "--" + self.name
        return "-" + self.short + ", --" + self.name


class Parser:
    def __init__(self, prog: str, description: str = ""):
        self.prog: str = prog
        self.description: str = description
        self.options: list[Option] = []
        self.positionals: list[str] = []

    def option(
        self,
        name: str,
        *,
        short: str = "",
        help: str = "",
        takes_value: bool = False,
        default: object = None,
        required: bool = False,
    ) -> None:
        self.options.append(
            Option(
                name,
                short=short,
                help=help,
                takes_value=takes_value,
                default=default,
                required=required,
            )
        )

    def positional(self, name: str) -> None:
        self.positionals.append(name)

    def find(self, token: str, is_short: bool) -> Option:
        for opt in self.options:
            if is_short and opt.short == token:
                return opt
            if not is_short and opt.name == token:
                return opt
        dash: str = "-" if is_short else "--"
        raise UsageError("unrecognized argument: " + dash + token)

    def parse(self, argv: list[str]) -> dict[str, object]:
        values: dict[str, object] = {}
        for opt in self.options:
            values[opt.name] = opt.default
        seen: list[str] = []
        rest: list[str] = []

        index: int = 0
        only_positional: bool = False
        while index < len(argv):
            token: str = argv[index]
            index += 1

            if only_positional or token == "-" or not token.startswith("-"):
                rest.append(token)
                continue
            if token == "--":
                only_positional = True
                continue

            if token.startswith("--"):
                body: str = token[2:]
                inline: str = ""
                has_inline: bool = "=" in body
                if has_inline:
                    name, _sep, inline = body.partition("=")
                    body = name
                opt: Option = self.find(body, False)
                if not opt.takes_value:
                    if has_inline:
                        raise UsageError("--" + body + " takes no value")
                    values[opt.name] = True
                elif has_inline:
                    values[opt.name] = inline
                elif index < len(argv):
                    values[opt.name] = argv[index]
                    index += 1
                else:
                    raise UsageError("--" + body + " needs a value")
                seen.append(opt.name)
                continue

            # A run of short flags: -v, -vq, or -o value.
            letters: str = token[1:]
            position: int = 0
            while position < len(letters):
                opt = self.find(letters[position], True)
                position += 1
                if not opt.takes_value:
                    values[opt.name] = True
                elif position < len(letters):
                    values[opt.name] = letters[position:]
                    position = len(letters)
                elif index < len(argv):
                    values[opt.name] = argv[index]
                    index += 1
                else:
                    raise UsageError("-" + opt.short + " needs a value")
                seen.append(opt.name)

        for opt in self.options:
            if opt.required and opt.name not in seen:
                raise UsageError("--" + opt.name + " is required")

        for slot, name in enumerate(self.positionals):
            if slot < len(rest):
                values[name] = rest[slot]
        values["_extra"] = rest[len(self.positionals) :]
        return values

    def usage(self) -> str:
        parts: list[str] = ["usage: " + self.prog]
        if len(self.options) > 0:
            parts.append("[options]")
        for name in self.positionals:
            parts.append("<" + name + ">")
        return " ".join(parts)

    def help(self) -> str:
        lines: list[str] = [self.usage()]
        if self.description != "":
            lines.append("")
            lines.append(self.description)
        if len(self.options) > 0:
            lines.append("")
            lines.append("options:")
            for opt in self.options:
                flag: str = opt.flag()
                if opt.takes_value:
                    flag = flag + " VALUE"
                lines.append("  " + flag.ljust(22) + opt.help)
        return "\n".join(lines)


def show(values: dict[str, object], names: list[str]) -> str:
    parts: list[str] = []
    for name in names:
        parts.append(name + "=" + str(values[name]))
    return " ".join(parts)


def build_parser() -> Parser:
    parser: Parser = Parser("pack", "Bundle a directory into an archive.")
    parser.option("verbose", short="v", help="say what is happening")
    parser.option(
        "output",
        short="o",
        help="write the archive here",
        takes_value=True,
        default="out.tar",
    )
    parser.option("level", help="compression level", takes_value=True, default="6")
    parser.positional("source")
    return parser


# ---------------------------------------------------------------- subcommands


def cmd_pack(argv: list[str]) -> int:
    values: dict[str, object] = build_parser().parse(argv)
    print("pack", show(values, ["source", "output", "level", "verbose"]))
    return 0


def cmd_list(argv: list[str]) -> int:
    print("list", len(argv), "argument(s)")
    return 0


def cmd_fail(argv: list[str]) -> int:
    print("fail")
    return 2


COMMANDS: dict[str, Callable[[list[str]], int]] = {
    "pack": cmd_pack,
    "list": cmd_list,
    "fail": cmd_fail,
}


def dispatch(argv: list[str]) -> int:
    if len(argv) == 0:
        print("usage: tool <command> [args]")
        return 2
    name: str = argv[0]
    if name not in COMMANDS:
        print("unknown command:", name)
        return 2
    handler = COMMANDS[name]
    return handler(argv[1:])


def main() -> None:
    print(build_parser().help())
    print()

    vectors: list[list[str]] = [
        ["src"],
        ["-v", "src"],
        ["--verbose", "--output", "a.tar", "src"],
        ["--output=b.tar", "src"],
        ["-o", "c.tar", "src"],
        ["-oc2.tar", "src"],
        ["-v", "--level", "9", "src", "extra1", "extra2"],
        ["--", "-not-a-flag"],
        ["--nope", "src"],
        ["--output"],
        ["--verbose=yes", "src"],
    ]
    parser: Parser = build_parser()
    for argv in vectors:
        try:
            values: dict[str, object] = parser.parse(argv)
            print(
                str(argv).ljust(46),
                show(values, ["source", "output", "level", "verbose", "_extra"]),
            )
        except UsageError as exc:
            print(str(argv).ljust(46), "error:", exc)

    print()
    for argv in [["pack", "-v", "here"], ["list", "a", "b"], ["fail"], ["nope"], []]:
        status: int = dispatch(argv)
        print(str(argv).ljust(24), "->", status)


main()
