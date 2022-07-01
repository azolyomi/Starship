import discord
from discord.ext import commands
import os
from permissions import errors
from permissions.errors import ERROR_NO_PERMISSION, ERROR_NOT_FOUND, ERROR_NOT_IN_GUILD
from util.constants import PREFIX
from database import db, ServerConfigs
from dotenv import load_dotenv
from util import logger
import logging

# logging setup
logging.basicConfig(level=logging.INFO)

# load bot token and other .env data
load_dotenv()

# create bot
BOT_TOKEN = os.getenv('BOT_TOKEN')



class StarshipBot(commands.Bot):
    def __init__(self):
        intents = discord.Intents.default()
        intents.message_content = True
        intents.dm_messages = True
        intents.guild_messages = True
        super().__init__(command_prefix=PREFIX, intents=intents)
        self.initial_extensions = [
            'cogs.help',
            'cogs.patreon',
            'cogs.ping',
            'cogs.setup',
            'cogs.config'
        ]

    async def setup_hook(self):
        for ext in self.initial_extensions:
            await self.load_extension(ext)
            logging.info("Loaded extension: {}".format(ext))

bot = StarshipBot()

@bot.event
async def on_ready():
    await bot.change_presence(activity=discord.Game(name=" {0}help | {0}patreon".format(PREFIX)))
    print('We have logged in as {0.user}'.format(bot))
    configs = list(db.ServerConfigs.find())
    for config in configs:
        ServerConfigs[config['guildID']] = config
    print('Server configs have been loaded. Count: ', len(ServerConfigs.keys()))

@bot.event
async def on_command_error(ctx, error):
    reply = ctx.message.reply if ctx.message is not None else ctx.send
    if isinstance(error, commands.CommandNotFound):
        await reply(ERROR_NOT_FOUND)
        return
    
    ancestry = "{0}{1}".format(ctx.command.full_parent_name, " " if len(ctx.command.full_parent_name) > 0 else "")
    usage = f'{bot.command_prefix}{ancestry}{ctx.command.name} {ctx.command.usage}'

    if isinstance(error, commands.NoPrivateMessage):
        await reply(ERROR_NOT_IN_GUILD)
    elif isinstance(error, errors.StarshipRoleMissingError):
        await reply(error.message)
    elif isinstance(error, errors.StarshipPermissionsError):
        await reply(error.message)
    elif isinstance(error, commands.MissingPermissions):
        await reply(ERROR_NO_PERMISSION)
    elif (isinstance(error, commands.MissingRequiredArgument)):
        await reply("You're missing a required argument. Usage: `{}`".format(usage))
    elif (isinstance(error, commands.BadArgument)):
        await reply(error)
    else:
        await logger.err(ctx, title= "Unknown Error", description=error)

bot.run(BOT_TOKEN)