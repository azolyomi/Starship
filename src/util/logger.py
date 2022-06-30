import discord
from database import ServerConfigs
from bot import bot
from util.constants import THEURUL_USER_ID

async def log(ctx, logtype = "Log", message = None, embed = None):
    if (message is None and embed is None): return

    if ctx.guild.id in ServerConfigs and ServerConfigs[ctx.guild.id]['log_channel_id'] is not None:
        channel = ctx.guild.get_channel(ServerConfigs[ctx.guild.id]['log_channel_id'])

        if (message):
            await channel.send(message)
            print("[{}] guild: [{}, {}] {}".format(logtype, ctx.guild.name, ctx.guild.id, message.replace("\n", " | ")))
        if (embed):
            await channel.send(embed=embed)
            print("[{}] guild: [{}, {}] {} | {}".format(logtype, ctx.guild.name, ctx.guild.id, embed.title, embed.description.replace("\n", " | ")))

async def info(ctx, title, description):
    await log(ctx, embed=discord.Embed(title=title, description=description, color=discord.Color.green()))

async def warn(ctx, title, description):
    await log(ctx, logtype="Warning", embed=discord.Embed(title=title, description=description, color=discord.Color.orange()))

async def err(ctx, title, description):
    await log(ctx, logtype="Error", embed=discord.Embed(title=title, description="Command: `{}` \nAuthor: {}\n\nError Message:\n```{}```".format(ctx.message.content, ctx.author.mention, description), color=discord.Color.red()))
    # also dm me the error
    urul = await bot.fetch_user(THEURUL_USER_ID)
    await urul.send(
        embed=discord.Embed(
            title=title, 
            description="Command: `{}` \nExecuted in: `[{},{}]`\n Author: {}\n\nError Message:\n```{}```".format(ctx.message.content, ctx.guild.name, ctx.guild.id, ctx.author.mention, description), 
            color=discord.Color.red()
        )
    )


