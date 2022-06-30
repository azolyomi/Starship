import discord
from database import ServerConfigs
from util.constants import THEURUL_USER_ID
import logging

# logging setup
logging.basicConfig(level=logging.INFO)



async def log(ctx, level = logging.INFO, message = None, embed = None):
    if (message is None and embed is None): return

    if (ctx.guild is None):
        # no guild, command was used in DM
        await ctx.message.reply(message, embed=embed)
        if (message):
            logging.log(level=20, msg=" DM: [{}, {}] {}".format(ctx.author.name, ctx.author.id, message.replace("\n", " | ")).replace("`", ""))
        if (embed):
            logging.log(level=level, msg=" DM: [{}, {}] {} | {}".format(ctx.author.name, ctx.author.id, embed.title, embed.description.replace("\n", " | ")).replace("`", ""))
        return

    # guild exists
    if ctx.guild.id in ServerConfigs and ServerConfigs[ctx.guild.id]['log_channel_id'] is not None:
        channel = ctx.guild.get_channel(ServerConfigs[ctx.guild.id]['log_channel_id'])

        if (message):
            await channel.send(message)
            logging.log(level=level, msg=" GUILD: [{}, {}] {}".format(ctx.guild.name, ctx.guild.id, message.replace("\n", " | ")).replace("`", ""))
        if (embed):
            await channel.send(embed=embed)
            logging.log(level=level, msg=" GUILD: [{}, {}] {} | {}".format(ctx.guild.name, ctx.guild.id, embed.title, embed.description.replace("\n", " | ")).replace("`", ""))

async def info(ctx, title, description):
    await log(ctx, embed=discord.Embed(title=title, description=description, color=discord.Color.green()))

async def warn(ctx, title, description):
    await log(ctx, level=logging.WARNING, embed=discord.Embed(title=title, description=description, color=discord.Color.orange()))

async def err(ctx, title, description):
    await log(ctx, level=logging.CRITICAL, embed=discord.Embed(title=title, description="Command: `{}` \nAuthor: {}\n\nError Message:\n```{}```".format(ctx.message.content, ctx.author.mention, description), color=discord.Color.red()))
    # also dm me the error
    urul = await ctx.bot.fetch_user(THEURUL_USER_ID)

    if (ctx.guild is not None):
        await urul.send(
            embed=discord.Embed(
                title=title, 
                description="Command: `{}` \nExecuted in: `[{},{}]`\n Author: {}\n\nError Message:\n```{}```".format(ctx.message.content, ctx.guild.name, ctx.guild.id, ctx.author.mention, description), 
                color=discord.Color.red()
            )
        )
    else:
        await urul.send(
            embed=discord.Embed(
                title=title, 
                description="Command: `{}`\n Author: {}\n\nError Message:\n```{}```".format(ctx.message.content, ctx.author.mention, description), 
                color=discord.Color.red()
            )
        )


