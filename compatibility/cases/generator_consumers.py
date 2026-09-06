# Generators as arguments to the eager builtins, including short-circuiting.
def numbers():
    yield 3
    yield 1
    yield 2


def words():
    yield "a"
    yield "b"


def loud():
    for i in range(4):
        print("visit", i)
        yield i


print(list(numbers()))
print(sorted(numbers()), sum(numbers()), max(numbers()), min(numbers()))
print(len(set(numbers())))
print(",".join(words()), list(words()))
print(any(loud()))
print(all(loud()))
