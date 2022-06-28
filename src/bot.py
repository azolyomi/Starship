import discord
from discord.ext import commands
import logging
import os
from database import db
from dotenv import load_dotenv

# load bot token and other .env data
load_dotenv()

# logging setup
logging.basicConfig(level=logging.INFO)

# create bot
BOT_TOKEN = os.getenv('BOT_TOKEN')
bot = commands.Bot(command_prefix='.')

# cog registration
bot.load_extension("cogs.help")
bot.load_extension("cogs.test")

@bot.event
async def on_ready():
    print('We have logged in as {0.user}'.format(bot))

bot.run(BOT_TOKEN)