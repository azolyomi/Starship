import discord
from discord.ext import commands
import logging
import os
from util.constants import PREFIX
from database import db, ServerConfigs
from dotenv import load_dotenv


# load bot token and other .env data
load_dotenv()

# logging setup
logging.basicConfig(level=logging.INFO)

# create bot
BOT_TOKEN = os.getenv('BOT_TOKEN')
bot = commands.Bot(command_prefix='!')

# cog registration
bot.load_extension("cogs.help")
bot.load_extension("cogs.patreon")
bot.load_extension("cogs.ping")
bot.load_extension("cogs.setup")
bot.load_extension("cogs.config")

@bot.event
async def on_ready():
    await bot.change_presence(activity=discord.Game(name=" {0}help | {0}patreon".format(PREFIX)))
    print('We have logged in as {0.user}'.format(bot))
    configs = list(db.ServerConfigs.find())
    for config in configs:
        ServerConfigs[config['guildID']] = config
    print('Server configs have been loaded. Count: ', len(ServerConfigs.keys()))
    

bot.run(BOT_TOKEN)