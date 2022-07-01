from util.constants import GLOBAL_ROLE_TYPES, PREFIX
from discord.ext.commands import Converter, BadArgument

class GlobalRoleType(Converter):
        async def convert(self, ctx, argument):
            if (argument in GLOBAL_ROLE_TYPES): return argument
            raise BadArgument("`roleType` must be one of `{}`".format(GLOBAL_ROLE_TYPES))