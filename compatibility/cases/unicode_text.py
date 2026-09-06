# String offsets are Unicode code points, across every UTF-8 width.
s = "éλ🙂"
print(len(s))
print(s[0], s[1], s[2])
print(s[1:3], s[::-1])
print([c for c in s])
print(s.find("λ"), s.count("é"), s.index("🙂"))
print("héllo héllo".rfind("é"), "héllo".partition("l"))
print("a,é,b".split(","), "--".join(["é", "λ"]))
print("xéx".strip("x"), "héllo".replace("é", "e"))
print(len("é".center(5, "*")), f"[{s:>6}]")
print(ord("🙂"), chr(0x1F642), len(chr(0x1F642)))
