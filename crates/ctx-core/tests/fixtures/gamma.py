class PyAlpha:
    def method(self):
        return "skip"


def py_helper():
    return PyAlpha()


async def async_worker():
    return await fetch_value()


if True:
    def nested_skip():
        return None
