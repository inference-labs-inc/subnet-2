from typing import Callable, ParamSpec, TypeVar
from functools import wraps
import time
import asyncio

P = ParamSpec("P")
T = TypeVar("T")


class RateLimiter:
    _instances = {}

    def __init__(self, period: float):
        self.period = period
        self.last_call = 0.0

    @classmethod
    def get_limiter(cls, func_name: str, period: float) -> "RateLimiter":
        if func_name not in cls._instances:
            cls._instances[func_name] = cls(period)
        return cls._instances[func_name]


def with_rate_limit(period: float):
    def decorator(func: Callable[P, T]) -> Callable[P, T]:
        limiter = RateLimiter.get_limiter(func.__name__, period)
        last_result = None

        if asyncio.iscoroutinefunction(func):

            @wraps(func)
            async def async_wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
                nonlocal last_result
                now = time.time()
                if now - limiter.last_call < period:
                    return last_result

                limiter.last_call = now
                last_result = await func(*args, **kwargs)
                return last_result

            return async_wrapper
        else:

            @wraps(func)
            def sync_wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
                nonlocal last_result
                now = time.time()
                if now - limiter.last_call < period:
                    return last_result

                limiter.last_call = now
                last_result = func(*args, **kwargs)
                return last_result

            return sync_wrapper

    return decorator
