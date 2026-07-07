# String handling: concat, repeat, compare, index, iterate
def shout(s: str) -> str:
    return s + "!" * 3

banner = "=" * 20
print(banner)
print(shout("hello"))

vowels = 0
for c in "the quick brown fox":
    if c == "a" or c == "e" or c == "i" or c == "o" or c == "u":
        vowels += 1
print("vowels:", vowels)

print("first:", "python"[0], "last:", "python"[-1])
print("abc" < "abd", str(3.5) + " as text")
print(banner)
