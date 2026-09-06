class Machine:
    def __init__(self) -> None:
        self.state = "idle"
        self.log: list[str] = []

    def step(self, event: str) -> str:
        if self.state == "idle" and event == "start":
            self.state = "running"
        elif self.state == "running" and event == "stop":
            self.state = "idle"
        elif event == "reset":
            self.state = "idle"
        else:
            self.log.append("ignored {} in {}".format(event, self.state))
        return self.state

m = Machine()
for e in ["start", "bogus", "stop", "reset"]:
    print(e, "->", m.step(e))
print(m.log)


class Box:
    def __init__(self) -> None:
        self.xs: list[int] = []
        self.d: dict[str, int] = {}
        self.opt: int | None = None

    def add(self, v: int) -> int:
        self.xs.append(v)
        self.d[str(v)] = v
        return len(self.xs)


b = Box()
print(b.xs, b.d, b.opt)
print(b.add(1), b.add(2))
print(b.xs, sorted(b.d.items()))
