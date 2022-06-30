import discord
from database import ServerConfigs

async def log(ctx, message = None, embed = None):
    if (message is None and embed is None): return

    if ctx.guild.id in ServerConfigs and ServerConfigs[ctx.guild.id]['log_channel_id'] is not None:
        channel = ctx.guild.get_channel(ServerConfigs[ctx.guild.id]['log_channel_id'])

        if (message):
            await channel.send(message)
            print("[Log: guild={0}] {1}".format(ctx.guild.name, message))
        if (embed):
            await channel.send(embed=embed)
            print("[Log: guild={0}] {1} | {2}".format(ctx.guild.name, embed.title, embed.description))

async def info(ctx, title, description):
    await log(ctx, embed=discord.Embed(title=title, description=description, color=discord.Color.green()))

async def warn(ctx, title, description):
    await log(ctx, embed=discord.Embed(title=title, description=description, color=discord.Color.orange()))

async def err(ctx, title, description):
    await log(ctx, embed=discord.Embed(title=title, description=description, color=discord.Color.red()))


