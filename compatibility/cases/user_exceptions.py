# User-defined exception classes: hierarchy, raise forms, builtin coexistence.
class AppError(Exception):
    pass


class NotFound(AppError):
    pass


try:
    raise NotFound("missing")
except AppError as e:
    print("base caught subclass:", e)

try:
    raise AppError("base")
except NotFound:
    print("WRONG")
except AppError as e:
    print("exact:", e)

try:
    raise NotFound("x")
except Exception as e:
    print("Exception caught user class:", e)

try:
    raise ValueError("v")
except AppError:
    print("WRONG")
except ValueError as e:
    print("builtin unaffected:", e)

try:
    raise NotFound
except NotFound as e:
    print("bare raise message length:", len(str(e)))
