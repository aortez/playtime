class SingleFlight:
    async def get(self, key, factory):
        raise NotImplementedError
