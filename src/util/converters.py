from util.constants import GLOBAL_ROLE_TYPES, GLOBAL_RAID_TYPES
from discord.ext.commands import Converter, BadArgument

class GlobalRoleType(Converter):
    async def convert(self, ctx, argument):
        if (argument in GLOBAL_ROLE_TYPES): return argument
        raise BadArgument("`roleType` must be one of `{}`".format(GLOBAL_ROLE_TYPES))

class GlobalRaidType(Converter):
    async def convert(self, ctx, argument):
        if (argument in GLOBAL_RAID_TYPES): return argument
        raise BadArgument("`raidType` must be one of `{}`".format(GLOBAL_RAID_TYPES))