# Bare `raise` re-raises the exception the enclosing handler caught.
class AppError(Exception):
    pass


def check(n: int) -> int:
    try:
        if n < 0:
            raise ValueError("negative")
        return n
    except ValueError:
        print("rethrowing")
        raise


try:
    try:
        raise ValueError("v")
    except ValueError:
        print("logging")
        raise
except ValueError as e:
    print("re-raised:", e)

try:
    try:
        raise AppError("custom")
    except AppError:
        raise
    finally:
        print("finally ran")
except AppError as e:
    print("type and message kept:", e)

print(check(3))
try:
    check(-1)
except ValueError as e:
    print("from function:", e)
