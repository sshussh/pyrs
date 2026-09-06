# Brace-style .format() and printf-style %, on literal format strings.
n = 7
print("{} and {}".format(1, 2))
print("{1}-{0}".format("a", "b"))
print("{name} is {age}".format(name="x", age=3))
print("mixed {} {k}".format(n, k="kw"))
print("{:.2f} {:>8}| {:05d}".format(3.14159, "hi", 42))
print("{!r} {{literal}} {}".format("q", 9))
print("%d-%s" % (3, "a"))
print("%s" % "solo")
print("%.2f %5d| %-5s|" % (3.14159, 42, "ab"))
print("%05d %x %o" % (42, 255, 8))
print("100%% done: %d" % 5)
