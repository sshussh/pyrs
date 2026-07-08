# File I/O: write a small report, then read it back three ways
path = "/tmp/pyrs-example-report.txt"

out = open(path, "w")
out.write("alpha 3\n")
out.write("beta 5\n")
out.write("gamma 2\n")
out.close()

whole = open(path)
print(len(whole.read()), "bytes")
whole.close()

names: list[str] = []
report = open(path)
for line in report.readlines():
    parts = line.split()
    names.append(parts[0])
report.close()
print(names)

first = open(path)
print("first line:", first.readline().strip())
first.close()
