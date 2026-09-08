"""__name__, sys.exit and print(file=...)."""
import sys


def main() -> None:
    print("main ran")


print("name is", __name__)
if __name__ == "__main__":
    main()

print("out one")
print("err one", file=sys.stderr)
print("out two", file=sys.stdout)
print("err two", "and more", sep="|", file=sys.stderr)

xs: list[int] = [1, 2]
print(str(xs), file=sys.stderr)
print(f"{xs}")

# The runner classifies a non-zero oracle exit as an oracle error, so the
# status itself is covered by cli/tests/module_and_streams.rs. What this
# probe still exercises is that buffered output survives the exit and that
# nothing after it runs.
for i in range(50):
    print("line", i)
sys.exit(0)
print("never runs")
