from util.constants import GLOBAL_ROLE_TYPES
from database import ServerConfigs
from discord.ext.commands import Converter, BadArgument

class GlobalRoleType(Converter):
    async def convert(self, ctx, argument):
        if (argument in GLOBAL_ROLE_TYPES): return argument
        raise BadArgument("`roleType` must be one of `{}`".format(GLOBAL_ROLE_TYPES))

class ValidNewCategoryID(Converter):
    async def convert(self, ctx, argument):
        categories = ServerConfigs[ctx.guild.id]["raiding"]["categories"].keys()
        if (len(argument) == 0): raise BadArgument("`category` cannot be empty.")
        elif (argument in categories): raise BadArgument("`category` cannot be the same as one of your existing categories: `{}`".format(categories))
        elif (len(argument) > 20): raise BadArgument("`category` must be less than 20 characters.")
        elif (" " in argument): raise BadArgument("`category` must not contain spaces.")
        return argument
    
class ExistingCategory(Converter):
    async def convert(self, ctx, argument):
        categories = ServerConfigs[ctx.guild.id]["raiding"]["categories"].keys()
        if (argument in categories): return argument
        raise BadArgument("`category` must be one of `{}`".format(categories))