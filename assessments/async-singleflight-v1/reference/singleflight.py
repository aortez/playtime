import asyncio


class SingleFlight:
    def __init__(self):
        self._tasks = {}

    async def get(self, key, factory):
        task = self._tasks.get(key)
        if task is None or task.done():
            async def run():
                return await factory()
            task = asyncio.create_task(run())
            self._tasks[key] = task

            def cleanup(done):
                if not done.cancelled():
                    done.exception()
                if self._tasks.get(key) is done:
                    del self._tasks[key]
            task.add_done_callback(cleanup)
        return await asyncio.shield(task)
